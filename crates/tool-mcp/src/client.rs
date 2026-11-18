//! MCP (Model Context Protocol) クライアント: JSON-RPC 2.0 で MCP サーバーとやりとりする。
//!
//! 運ぶ手段 ([`Transport`]) は2つ。
//! - [`Stdio`]: サーバーを子プロセスとして起動し、stdin/stdout に1行1メッセージの JSON を流す。
//! - [`Http`]: Streamable HTTP。メッセージごとに POST し、応答は JSON か SSE で受ける。
//!
//! 使うのは `initialize` → `notifications/initialized` → `tools/list` / `tools/call` だけ。
//! サーバーからのリクエストは `ping` にだけ答え、それ以外は「無い」と返す。

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio as ProcessStdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use letsharness_kernel::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const PLUGIN: &str = "mcp";

/// 送る protocol version。サーバーが別の版を返しても、使うメソッドは同じなのでそのまま続ける。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// 標準エラーを何行まで覚えておくか (サーバーが落ちたときのエラーに添える)。
const STDERR_TAIL: usize = 20;

fn fail(server: &str, msg: impl std::fmt::Display) -> Error {
	Error::plugin(PLUGIN, format!("{server}: {msg}"))
}

/// JSON-RPC のメッセージを運ぶ手段。
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
	/// リクエストを送って応答の `result` を返す。`error` は `Err`。
	async fn request(&self, method: &str, params: Value) -> Result<Value>;

	/// 応答の無い通知を送る。
	async fn notify(&self, method: &str, params: Value) -> Result<()>;

	/// `initialize` で決まった protocol version (HTTP はヘッダに付ける)。
	fn negotiated(&self, _version: &str) {}

	/// もう使えない (サーバーが終了したなど)。
	fn is_closed(&self) -> bool {
		false
	}
}

#[derive(Debug, Deserialize)]
struct RpcError {
	code: i64,
	message: String,
}

/// 応答の `result` / `error` を取り出す。
fn into_result(server: &str, msg: &Value) -> Result<Value> {
	if let Some(err) = msg.get("error") {
		return Err(match serde_json::from_value::<RpcError>(err.clone()) {
			Ok(e) => fail(server, format!("{} (code {})", e.message, e.code)),
			Err(_) => fail(server, err),
		});
	}
	Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

/// サーバーからのリクエストへの応答。`ping` にだけ答える。
fn reply_to_server(id: &Value, method: &str) -> Value {
	match method {
		"ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
		_ => json!({
			"jsonrpc": "2.0",
			"id": id,
			"error": { "code": -32601, "message": format!("method not found: {method}") },
		}),
	}
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

type Reply = oneshot::Sender<Result<Value>>;

#[derive(Default)]
struct State {
	pending: HashMap<u64, Reply>,
	/// 閉じた理由。
	closed: Option<String>,
}

/// 子プロセスのサーバー (または任意の読み書き口) と1行1メッセージでやりとりする。
pub struct Stdio {
	server: String,
	out: mpsc::UnboundedSender<String>,
	state: Arc<Mutex<State>>,
	next_id: AtomicU64,
	_child: Option<tokio::process::Child>,
}

/// 待っている応答を、呼び出しが捨てられたら (タイムアウトなど) 忘れる。
struct PendingGuard<'a> {
	state: &'a Mutex<State>,
	id: u64,
}

impl Drop for PendingGuard<'_> {
	fn drop(&mut self) {
		if let Ok(mut state) = self.state.lock() {
			state.pending.remove(&self.id);
		}
	}
}

impl Stdio {
	/// `command` を起動してつなぐ。標準エラーは捨てるが、最後の数行は落ちたときのエラーに添える。
	pub fn spawn(
		server: &str,
		command: &str,
		args: &[String],
		env: &HashMap<String, String>,
		cwd: Option<&Path>,
	) -> Result<Self> {
		let mut cmd = tokio::process::Command::new(command);
		cmd.args(args)
			.envs(env)
			.stdin(ProcessStdio::piped())
			.stdout(ProcessStdio::piped())
			.stderr(ProcessStdio::piped())
			.kill_on_drop(true);
		if let Some(dir) = cwd {
			cmd.current_dir(dir);
		}
		let mut child = cmd.spawn().map_err(|e| fail(server, format!("{command}: {e}")))?;
		let stdin = child.stdin.take().expect("piped");
		let stdout = child.stdout.take().expect("piped");
		let stderr = child.stderr.take().expect("piped");

		let tail = Arc::new(Mutex::new(VecDeque::new()));
		let lines = tail.clone();
		tokio::spawn(async move {
			let mut reader = BufReader::new(stderr).lines();
			while let Ok(Some(line)) = reader.next_line().await {
				let mut lines = lines.lock().unwrap();
				if lines.len() == STDERR_TAIL {
					lines.pop_front();
				}
				lines.push_back(line);
			}
		});
		let mut this = Self::connect(server, stdout, stdin, Some(tail));
		this._child = Some(child);
		Ok(this)
	}

	/// 読み書き口につなぐ (テスト用にも使う)。
	pub fn connect(
		server: &str,
		reader: impl AsyncRead + Send + Unpin + 'static,
		mut writer: impl AsyncWrite + Send + Unpin + 'static,
		stderr: Option<Arc<Mutex<VecDeque<String>>>>,
	) -> Self {
		let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
		tokio::spawn(async move {
			while let Some(mut line) = out_rx.recv().await {
				line.push('\n');
				if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
					break;
				}
			}
		});

		let state = Arc::new(Mutex::new(State::default()));
		let (reader_state, replies, name) = (state.clone(), out.clone(), server.to_owned());
		tokio::spawn(async move {
			let mut lines = BufReader::new(reader).lines();
			let reason = loop {
				let line = match lines.next_line().await {
					Ok(Some(line)) => line,
					Ok(None) => break "サーバーが終了しました".to_owned(),
					Err(e) => break format!("read: {e}"),
				};
				if line.trim().is_empty() {
					continue;
				}
				let Ok(msg) = serde_json::from_str::<Value>(&line) else {
					eprintln!("warning: mcp {name}: JSON ではない行を無視します: {line}");
					continue;
				};
				match (msg.get("id"), msg.get("method").and_then(Value::as_str)) {
					// サーバーからのリクエスト
					(Some(id), Some(method)) => {
						let _ = replies.send(reply_to_server(id, method).to_string());
					}
					// 応答 (こちらの id は数値)
					(Some(id), None) => {
						let reply = id
							.as_u64()
							.and_then(|id| reader_state.lock().unwrap().pending.remove(&id));
						if let Some(reply) = reply {
							let _ = reply.send(into_result(&name, &msg));
						}
					}
					// 通知 (ツール一覧の変更・ログなど) は使わない
					_ => {}
				}
			};
			let reason = match stderr.map(|t| t.lock().unwrap().iter().cloned().collect::<Vec<_>>()) {
				Some(tail) if !tail.is_empty() => format!("{reason}\n{}", tail.join("\n")),
				_ => reason,
			};
			let mut state = reader_state.lock().unwrap();
			for (_, reply) in state.pending.drain() {
				let _ = reply.send(Err(fail(&name, &reason)));
			}
			state.closed = Some(reason);
		});

		Self {
			server: server.to_owned(),
			out,
			state,
			next_id: AtomicU64::new(1),
			_child: None,
		}
	}

	fn send(&self, msg: Value) -> Result<()> {
		self.out
			.send(msg.to_string())
			.map_err(|_| fail(&self.server, "書き込めません"))
	}
}

#[async_trait::async_trait]
impl Transport for Stdio {
	async fn request(&self, method: &str, params: Value) -> Result<Value> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let (tx, rx) = oneshot::channel();
		{
			let mut state = self.state.lock().unwrap();
			if let Some(reason) = &state.closed {
				return Err(fail(&self.server, reason));
			}
			state.pending.insert(id, tx);
		}
		let _guard = PendingGuard { state: &self.state, id };
		self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
		rx.await.map_err(|_| fail(&self.server, "応答がありません"))?
	}

	async fn notify(&self, method: &str, params: Value) -> Result<()> {
		self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
	}

	fn is_closed(&self) -> bool {
		self.state.lock().unwrap().closed.is_some()
	}
}

// ---------------------------------------------------------------------------
// Streamable HTTP
// ---------------------------------------------------------------------------

/// Streamable HTTP のサーバー。メッセージごとに `url` へ POST する。
pub struct Http {
	server: String,
	client: reqwest::Client,
	url: String,
	headers: Vec<(String, String)>,
	/// `initialize` の応答の `Mcp-Session-Id`。
	session: Mutex<Option<String>>,
	protocol: Mutex<Option<String>>,
	next_id: AtomicU64,
}

impl Http {
	pub fn new(server: &str, url: &str, headers: Vec<(String, String)>) -> Result<Self> {
		let client = reqwest::Client::builder()
			.user_agent("letsharness/0.1")
			.build()
			.map_err(|e| fail(server, e))?;
		Ok(Self {
			server: server.to_owned(),
			client,
			url: url.to_owned(),
			headers,
			session: Mutex::new(None),
			protocol: Mutex::new(None),
			next_id: AtomicU64::new(1),
		})
	}

	async fn post(&self, body: &Value) -> Result<reqwest::Response> {
		let mut req = self
			.client
			.post(&self.url)
			.header("accept", "application/json, text/event-stream")
			.json(body);
		for (k, v) in &self.headers {
			req = req.header(k, v);
		}
		if let Some(id) = self.session.lock().unwrap().clone() {
			req = req.header("mcp-session-id", id);
		}
		if let Some(v) = self.protocol.lock().unwrap().clone() {
			req = req.header("mcp-protocol-version", v);
		}
		let res = req.send().await.map_err(|e| fail(&self.server, e))?;
		if let Some(id) = res.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
			*self.session.lock().unwrap() = Some(id.to_owned());
		}
		if !res.status().is_success() {
			let status = res.status().as_u16();
			let body = res.text().await.unwrap_or_default();
			return Err(fail(&self.server, format!("HTTP {status}: {body}")));
		}
		Ok(res)
	}
}

/// SSE の本文から、`id` への応答を探す。
fn find_in_sse(server: &str, text: &str, id: u64) -> Option<Result<Value>> {
	text.replace("\r\n", "\n").split("\n\n").find_map(|event| {
		let data: Vec<&str> = event
			.lines()
			.filter_map(|l| l.strip_prefix("data:"))
			.map(str::trim_start)
			.collect();
		let msg: Value = serde_json::from_str(&data.join("\n")).ok()?;
		(msg.get("id").and_then(Value::as_u64) == Some(id) && msg.get("method").is_none())
			.then(|| into_result(server, &msg))
	})
}

#[async_trait::async_trait]
impl Transport for Http {
	async fn request(&self, method: &str, params: Value) -> Result<Value> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let res = self
			.post(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
			.await?;
		let sse = res
			.headers()
			.get("content-type")
			.and_then(|v| v.to_str().ok())
			.is_some_and(|t| t.starts_with("text/event-stream"));
		if !sse {
			let msg: Value = res.json().await.map_err(|e| fail(&self.server, e))?;
			return into_result(&self.server, &msg);
		}
		// 応答が来た時点で読むのをやめる (サーバーはその後もストリームを開けておけるため)。
		let mut body = res.bytes_stream();
		let mut text = String::new();
		while let Some(chunk) = body.next().await {
			text.push_str(&String::from_utf8_lossy(&chunk.map_err(|e| fail(&self.server, e))?));
			if let Some(found) = find_in_sse(&self.server, &text, id) {
				return found;
			}
		}
		find_in_sse(&self.server, &text, id).unwrap_or_else(|| Err(fail(&self.server, "応答がありません")))
	}

	async fn notify(&self, method: &str, params: Value) -> Result<()> {
		self.post(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
			.await
			.map(drop)
	}

	fn negotiated(&self, version: &str) {
		*self.protocol.lock().unwrap() = Some(version.to_owned());
	}
}

// ---------------------------------------------------------------------------
// クライアント
// ---------------------------------------------------------------------------

/// `tools/list` の1件。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
	pub name: String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default)]
	pub input_schema: Value,
}

/// 初期化を済ませた1つの MCP サーバー。
pub struct McpClient {
	pub server: String,
	transport: Box<dyn Transport>,
	timeout: Duration,
}

impl McpClient {
	/// `initialize` を済ませる。`startup` までに終わらなければ `Err`。`timeout` は以降の呼び出しの上限。
	pub async fn connect(
		server: &str,
		transport: Box<dyn Transport>,
		startup: Duration,
		timeout: Duration,
	) -> Result<Self> {
		let client = Self {
			server: server.to_owned(),
			transport,
			timeout,
		};
		let params = json!({
			"protocolVersion": PROTOCOL_VERSION,
			"capabilities": {},
			"clientInfo": { "name": "letsharness", "version": env!("CARGO_PKG_VERSION") },
		});
		let init = client.call_within("initialize", params, startup).await?;
		if let Some(v) = init.get("protocolVersion").and_then(Value::as_str) {
			client.transport.negotiated(v);
		}
		client.transport.notify("notifications/initialized", json!({})).await?;
		Ok(client)
	}

	async fn call_within(&self, method: &str, params: Value, limit: Duration) -> Result<Value> {
		tokio::time::timeout(limit, self.transport.request(method, params))
			.await
			.map_err(|_| {
				fail(
					&self.server,
					format!("{method}: {}秒で応答がありません", limit.as_secs()),
				)
			})?
	}

	pub fn is_closed(&self) -> bool {
		self.transport.is_closed()
	}

	/// すべてのツール (`nextCursor` を追ってページをつなぐ)。
	pub async fn list_tools(&self) -> Result<Vec<ToolInfo>> {
		let mut tools = Vec::new();
		let mut cursor: Option<String> = None;
		loop {
			let params = match &cursor {
				Some(c) => json!({ "cursor": c }),
				None => json!({}),
			};
			let page = self.call_within("tools/list", params, self.timeout).await?;
			let list = page.get("tools").cloned().unwrap_or_else(|| json!([]));
			let list: Vec<ToolInfo> =
				serde_json::from_value(list).map_err(|e| fail(&self.server, format!("tools/list: {e}")))?;
			tools.extend(list);
			cursor = page
				.get("nextCursor")
				.and_then(Value::as_str)
				.filter(|c| !c.is_empty())
				.map(str::to_owned);
			if cursor.is_none() {
				return Ok(tools);
			}
		}
	}

	/// `tools/call` の `result` (`content` / `isError` / `structuredContent`)。
	pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
		let arguments = if arguments.is_null() { json!({}) } else { arguments };
		self.call_within(
			"tools/call",
			json!({ "name": name, "arguments": arguments }),
			self.timeout,
		)
		.await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 1行ずつ読み、台本どおりに答える MCP サーバー (テスト用)。
	async fn fake_server(reader: tokio::io::DuplexStream, mut writer: tokio::io::DuplexStream) {
		let mut lines = BufReader::new(reader).lines();
		while let Ok(Some(line)) = lines.next_line().await {
			let msg: Value = serde_json::from_str(&line).unwrap();
			let Some(id) = msg.get("id").cloned() else { continue };
			if msg.get("method").is_none() {
				continue; // こちらの ping への応答
			}
			let result = match msg["method"].as_str().unwrap() {
				"initialize" => json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
				"tools/list" if msg["params"].get("cursor").is_none() => json!({
					"tools": [{ "name": "a", "inputSchema": { "type": "object" } }],
					"nextCursor": "p2",
				}),
				"tools/list" => json!({ "tools": [{ "name": "b", "description": "B" }] }),
				"tools/call" => {
					// 応答の前にクライアントへ ping を送る
					let ping = json!({ "jsonrpc": "2.0", "id": "s1", "method": "ping" });
					writer.write_all(format!("{ping}\n").as_bytes()).await.unwrap();
					json!({ "content": [{ "type": "text", "text": msg["params"]["arguments"].to_string() }] })
				}
				"hang" => continue,
				_ => {
					let err = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "nope" } });
					writer.write_all(format!("{err}\n").as_bytes()).await.unwrap();
					continue;
				}
			};
			let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
			writer.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
		}
	}

	async fn client() -> McpClient {
		// 向きごとに1本ずつ
		let (to_server_w, to_server_r) = tokio::io::duplex(4096);
		let (to_client_w, to_client_r) = tokio::io::duplex(4096);
		tokio::spawn(fake_server(to_server_r, to_client_w));
		let transport = Stdio::connect("fake", to_client_r, to_server_w, None);
		McpClient::connect(
			"fake",
			Box::new(transport),
			Duration::from_secs(5),
			Duration::from_millis(200),
		)
		.await
		.unwrap()
	}

	#[tokio::test]
	async fn lists_every_page_and_calls_tools() {
		let c = client().await;
		let names: Vec<String> = c.list_tools().await.unwrap().into_iter().map(|t| t.name).collect();
		assert_eq!(names, ["a", "b"]);
		let out = c.call_tool("a", json!({ "x": 1 })).await.unwrap();
		assert_eq!(out["content"][0]["text"], r#"{"x":1}"#);
	}

	#[tokio::test]
	async fn errors_and_timeouts_come_back_as_errors() {
		let c = client().await;
		let err = c.call_within("other", json!({}), c.timeout).await.unwrap_err();
		assert!(err.to_string().contains("nope (code -32601)"), "{err}");
		let err = c.call_within("hang", json!({}), c.timeout).await.unwrap_err();
		assert!(err.to_string().contains("応答がありません"), "{err}");
		assert!(!c.is_closed());
	}

	/// Streamable HTTP の小さなサーバー: `initialize` は JSON とセッション ID、以降は SSE で答え、
	/// セッション ID と protocol version のヘッダが無ければ 400 にする。
	async fn http_server() -> String {
		use tokio::io::AsyncReadExt;
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("http://{}/mcp", listener.local_addr().unwrap());
		tokio::spawn(async move {
			loop {
				let Ok((mut sock, _)) = listener.accept().await else {
					return;
				};
				tokio::spawn(async move {
					let mut buf = Vec::new();
					let mut chunk = [0u8; 4096];
					let (head, body) = loop {
						let n = sock.read(&mut chunk).await.unwrap();
						buf.extend_from_slice(&chunk[..n]);
						let text = String::from_utf8_lossy(&buf).into_owned();
						if let Some((head, body)) = text.split_once("\r\n\r\n") {
							let len = head
								.lines()
								.find_map(|l| {
									l.to_ascii_lowercase()
										.strip_prefix("content-length:")
										.map(|v| v.trim().parse::<usize>().unwrap())
								})
								.unwrap_or(0);
							if body.len() >= len {
								break (head.to_ascii_lowercase(), body.to_owned());
							}
						}
					};
					let msg: Value = serde_json::from_str(&body).unwrap();
					let respond = |status: &str, ctype: &str, extra: &str, body: String| {
						format!(
							"HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
							body.len()
						)
					};
					let method = msg["method"].as_str().unwrap_or_default();
					let reply =
						if method == "initialize" {
							let result = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "protocolVersion": "2025-06-18" } });
							respond(
								"200 OK",
								"application/json",
								"mcp-session-id: s-1\r\n",
								result.to_string(),
							)
						} else if !head.contains("mcp-session-id: s-1")
							|| !head.contains("mcp-protocol-version: 2025-06-18")
						{
							respond("400 Bad Request", "text/plain", "", "missing session".into())
						} else if msg.get("id").is_none() {
							respond("202 Accepted", "text/plain", "", String::new())
						} else {
							let result = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "tools": [{ "name": "remote" }] } });
							respond(
								"200 OK",
								"text/event-stream",
								"",
								format!("event: message\ndata: {result}\n\n"),
							)
						};
					let _ = sock.write_all(reply.as_bytes()).await;
				});
			}
		});
		url
	}

	#[tokio::test]
	async fn streamable_http_keeps_the_session() {
		let url = http_server().await;
		let transport = Http::new("web", &url, vec![]).unwrap();
		let c = McpClient::connect(
			"web",
			Box::new(transport),
			Duration::from_secs(5),
			Duration::from_secs(5),
		)
		.await
		.unwrap();
		let names: Vec<String> = c.list_tools().await.unwrap().into_iter().map(|t| t.name).collect();
		assert_eq!(names, ["remote"]);
	}

	#[test]
	fn sse_bodies_are_searched_for_the_reply() {
		let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
			data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
		assert_eq!(find_in_sse("s", body, 7).unwrap().unwrap(), json!({ "ok": true }));
		assert!(find_in_sse("s", body, 8).is_none());
	}
}
