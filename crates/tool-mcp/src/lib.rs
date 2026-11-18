//! MCP クライアント: MCP サーバーのツールを LetsHarness のツール ([`Tool`]) として見せる。
//!
//! どのサーバーにつなぐかは cfg で決める (どちらも無ければツールは無い)。
//! - `mcp_servers`: サーバー名 → 設定の表 (パッチ層の `config` に書く)。`-o` で渡すなら JSON の文字列。
//! - `mcp_config`: `.mcp.json` 形 (`{"mcpServers": {...}}`) のファイル。Claude Code などと同じ書き方。
//!
//! ```toml
//! # <home>/profiles/default/patch.toml
//! [[plugins]]
//! id = "mcp"
//! config.mcp_servers.fs = { command = "npx", args = ["-y", "@modelcontextprotocol/server-filesystem", "."] }
//! config.mcp_servers.docs = { type = "http", url = "https://example.com/mcp", headers = { Authorization = "Bearer ${DOCS_TOKEN}" } }
//! ```
//!
//! - 1つのサーバーの設定は `command` / `args` / `env` / `cwd` (stdio) か `url` / `headers` (Streamable HTTP)。
//!   `timeout_secs` (既定 300) は1回の呼び出しの上限、`startup_timeout_secs` (既定 60) は `initialize` の上限。
//!   文字列の中の `${VAR}` / `${VAR:-既定}` は環境変数に置き換える。
//! - ツール名は `mcp__<サーバー名>__<ツール名>` (英数字・`_`・`-` 以外は `_`)。
//! - stdio のサーバーの作業ディレクトリは `cwd` (相対パスは `cfg.workdir` から)、無ければ `cfg.workdir`。
//! - つなげないサーバーがあれば `create` は `Err` (黙って飛ばさない)。同じ設定のサーバーへの接続は使い回す。

mod client;

pub use client::{Http, McpClient, PROTOCOL_VERSION, Stdio, ToolInfo, Transport};

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use letsharness_kernel::{Error, PluginServer, Result, Tool, ToolFactory, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 60;
/// 各社 API のツール名の上限。
const MAX_TOOL_NAME: usize = 64;

pub fn server() -> PluginServer {
	PluginServer::new().tool(McpFactory::default())
}

/// 1つの MCP サーバーの設定。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
	/// `stdio` (既定) / `http` (`streamable-http` も可)。`sse` (古い HTTP+SSE) は未対応。
	#[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
	pub transport: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub command: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub args: Vec<String>,
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub env: BTreeMap<String, String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cwd: Option<PathBuf>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub url: Option<String>,
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub headers: BTreeMap<String, String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timeout_secs: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub startup_timeout_secs: Option<u64>,
}

fn config_err(msg: impl std::fmt::Display) -> Error {
	Error::Config(format!("mcp: {msg}"))
}

/// cfg からサーバーの一覧 (名前順) を読む。`mcp_config` のファイル → `mcp_servers` の順に重ね、同じ名前は後が勝つ。
pub fn servers(cfg: &Value) -> Result<BTreeMap<String, ServerConfig>> {
	let mut out = BTreeMap::new();
	if let Some(path) = cfg.get("mcp_config").and_then(Value::as_str) {
		let src = std::fs::read_to_string(path).map_err(|e| config_err(format!("mcp_config {path}: {e}")))?;
		let file: Value = serde_json::from_str(&src).map_err(|e| config_err(format!("mcp_config {path}: {e}")))?;
		let list = file.get("mcpServers").cloned().unwrap_or_else(|| serde_json::json!({}));
		out.extend(parse_servers(list, &format!("mcp_config {path}"))?);
	}
	match cfg.get("mcp_servers") {
		None | Some(Value::Null) => {}
		// `-o mcp_servers='{"x": {...}}'`
		Some(Value::String(s)) => {
			let list = serde_json::from_str(s).map_err(|e| config_err(format!("mcp_servers: {e}")))?;
			out.extend(parse_servers(list, "mcp_servers")?);
		}
		Some(list) => out.extend(parse_servers(list.clone(), "mcp_servers")?),
	}
	Ok(out)
}

fn parse_servers(list: Value, source: &str) -> Result<BTreeMap<String, ServerConfig>> {
	let list: BTreeMap<String, ServerConfig> =
		serde_json::from_value(list).map_err(|e| config_err(format!("{source}: {e}")))?;
	for (name, server) in &list {
		match (server.transport.as_deref(), &server.command, &server.url) {
			(None | Some("stdio"), Some(_), None) | (Some("http" | "streamable-http") | None, None, Some(_)) => {}
			(Some("sse"), ..) => {
				return Err(config_err(format!(
					"{source}: {name}: type = \"sse\" (古い HTTP+SSE) は未対応です。Streamable HTTP の url を使ってください"
				)));
			}
			_ => {
				return Err(config_err(format!(
					"{source}: {name}: `command` (stdio) か `url` (http) のどちらか1つが必要です"
				)));
			}
		}
	}
	Ok(list)
}

/// `${VAR}` / `${VAR:-既定}` を環境変数に置き換える。無い変数は空 (既定があれば既定)。
pub fn expand(s: &str, env: impl Fn(&str) -> Option<String>) -> String {
	let mut out = String::new();
	let mut rest = s;
	while let Some(start) = rest.find("${") {
		let Some(len) = rest[start..].find('}') else { break };
		out.push_str(&rest[..start]);
		let inner = &rest[start + 2..start + len];
		let (name, default) = inner.split_once(":-").unwrap_or((inner, ""));
		out.push_str(&env(name).unwrap_or_else(|| default.to_owned()));
		rest = &rest[start + len + 1..];
	}
	out.push_str(rest);
	out
}

impl ServerConfig {
	/// 文字列の `${VAR}` を置き換え、`cwd` を `workdir` から解決したもの。
	fn resolved(&self, workdir: Option<&Path>) -> Self {
		let env = |name: &str| std::env::var(name).ok();
		let x = |s: &String| expand(s, env);
		let cwd = match (&self.cwd, workdir) {
			(Some(cwd), Some(dir)) => Some(dir.join(cwd)),
			(Some(cwd), None) => Some(cwd.clone()),
			(None, dir) => dir.map(Path::to_path_buf),
		};
		Self {
			transport: self.transport.clone(),
			command: self.command.as_ref().map(x),
			args: self.args.iter().map(x).collect(),
			env: self.env.iter().map(|(k, v)| (k.clone(), x(v))).collect(),
			cwd,
			url: self.url.as_ref().map(x),
			headers: self.headers.iter().map(|(k, v)| (k.clone(), x(v))).collect(),
			timeout_secs: self.timeout_secs,
			startup_timeout_secs: self.startup_timeout_secs,
		}
	}

	async fn connect(&self, name: &str) -> Result<McpClient> {
		let transport: Box<dyn Transport> = match (&self.command, &self.url) {
			(Some(command), _) => {
				let env: HashMap<String, String> = self.env.clone().into_iter().collect();
				Box::new(Stdio::spawn(name, command, &self.args, &env, self.cwd.as_deref())?)
			}
			(None, Some(url)) => {
				let headers = self.headers.clone().into_iter().collect();
				Box::new(Http::new(name, url, headers)?)
			}
			(None, None) => unreachable!("parse_servers が弾く"),
		};
		let secs = |v: Option<u64>, default| Duration::from_secs(v.unwrap_or(default));
		McpClient::connect(
			name,
			transport,
			secs(self.startup_timeout_secs, DEFAULT_STARTUP_TIMEOUT_SECS),
			secs(self.timeout_secs, DEFAULT_TIMEOUT_SECS),
		)
		.await
	}
}

/// `mcp__<server>__<tool>`。各社 API が受け付ける文字 (英数字・`_`・`-`) に寄せ、長すぎれば切る。
pub fn tool_name(server: &str, tool: &str) -> String {
	let name: String = format!("mcp__{server}__{tool}")
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
				c
			} else {
				'_'
			}
		})
		.collect();
	name.chars().take(MAX_TOOL_NAME).collect()
}

/// `tools/call` の結果をモデルに返すテキストにする。
///
/// `text` はそのまま、`resource` は中身のテキスト (無ければ URI)、画像・音声などは種類だけを書く。
/// テキストが1つも無く `structuredContent` があれば、その JSON を返す。
pub fn to_output(result: &Value) -> ToolOutput {
	let items = result
		.get("content")
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or(&[]);
	let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
	let mut parts: Vec<String> = items
		.iter()
		.map(|item| match item.get("type").and_then(Value::as_str) {
			Some("text") => s(item, "text"),
			Some("resource") => {
				let r = item.get("resource").cloned().unwrap_or_default();
				r.get("text")
					.and_then(Value::as_str)
					.map(str::to_owned)
					.unwrap_or_else(|| format!("[resource: {}]", s(&r, "uri")))
			}
			Some("resource_link") => format!("[resource_link: {}]", s(item, "uri")),
			Some(kind) => format!("[{kind}: {}]", s(item, "mimeType")),
			None => item.to_string(),
		})
		.collect();
	if parts.iter().all(|p| p.is_empty())
		&& let Some(structured) = result.get("structuredContent")
	{
		parts = vec![structured.to_string()];
	}
	let content = parts.join("\n");
	if result.get("isError").and_then(Value::as_bool) == Some(true) {
		ToolOutput::error(content)
	} else {
		ToolOutput::ok(content)
	}
}

/// MCP サーバーの1つのツール。
struct McpTool {
	client: Arc<McpClient>,
	/// サーバーでの名前。
	remote: String,
	spec: ToolSpec,
}

#[async_trait::async_trait]
impl Tool for McpTool {
	fn spec(&self) -> ToolSpec {
		self.spec.clone()
	}

	async fn call(&self, args: Value) -> ToolOutput {
		match self.client.call_tool(&self.remote, args).await {
			Ok(result) => to_output(&result),
			Err(e) => ToolOutput::error(e.to_string()),
		}
	}
}

/// サーバーへの接続を、同じ設定 (名前と解決後の設定) なら使い回す。
#[derive(Default)]
struct McpFactory {
	clients: Mutex<HashMap<String, Arc<McpClient>>>,
}

impl McpFactory {
	async fn client(&self, name: &str, server: &ServerConfig) -> Result<Arc<McpClient>> {
		let key = serde_json::to_string(&(name, server)).map_err(config_err)?;
		let mut clients = self.clients.lock().await;
		if let Some(client) = clients.get(&key).filter(|c| !c.is_closed()) {
			return Ok(client.clone());
		}
		let client = Arc::new(server.connect(name).await?);
		clients.insert(key, client.clone());
		Ok(client)
	}
}

#[async_trait::async_trait]
impl ToolFactory for McpFactory {
	fn name(&self) -> &'static str {
		"mcp"
	}

	async fn create(&self, cfg: &Value) -> Result<Vec<Arc<dyn Tool>>> {
		let workdir = cfg.get("workdir").and_then(Value::as_str).map(PathBuf::from);
		let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
		let servers = servers(cfg)?;
		// サーバーの起動は待ち時間が長いので並べて待つ。
		let workdir = workdir.as_deref();
		let connected = futures_util::future::join_all(
			servers
				.iter()
				.map(|(name, server)| async move { (name, self.client(name, &server.resolved(workdir)).await) }),
		)
		.await;
		for (name, client) in connected {
			let client = client?;
			for info in client.list_tools().await? {
				let description = info.description.or(info.title).unwrap_or_default();
				let input_schema = match info.input_schema {
					Value::Null => serde_json::json!({ "type": "object" }),
					schema => schema,
				};
				tools.push(Arc::new(McpTool {
					client: client.clone(),
					spec: ToolSpec {
						name: tool_name(name, &info.name),
						description,
						input_schema,
					},
					remote: info.name,
				}));
			}
		}
		Ok(tools)
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn servers_come_from_the_file_and_the_cfg() {
		let file = std::env::temp_dir().join(format!("lh-mcp-{}.json", std::process::id()));
		std::fs::write(
			&file,
			r#"{"mcpServers": {"a": {"command": "x", "args": ["1"]}, "b": {"type": "http", "url": "http://h/mcp"}}}"#,
		)
		.unwrap();
		let cfg = json!({
			"mcp_config": file.to_str().unwrap(),
			"mcp_servers": r#"{"a": {"command": "y"}}"#,
		});
		let list = servers(&cfg).unwrap();
		assert_eq!(list.keys().collect::<Vec<_>>(), ["a", "b"]);
		assert_eq!(list["a"].command.as_deref(), Some("y"), "mcp_servers wins");
		assert_eq!(list["b"].url.as_deref(), Some("http://h/mcp"));
		assert!(servers(&json!({})).unwrap().is_empty());
		std::fs::remove_file(&file).unwrap();
	}

	#[test]
	fn bad_servers_fail_loud() {
		let bad = |v: Value| servers(&json!({ "mcp_servers": v })).unwrap_err().to_string();
		assert!(bad(json!({ "x": {} })).contains("`command` (stdio) か `url` (http)"));
		assert!(bad(json!({ "x": { "command": "a", "url": "b" } })).contains("どちらか1つ"));
		assert!(bad(json!({ "x": { "type": "sse", "url": "b" } })).contains("未対応"));
		assert!(servers(&json!({ "mcp_config": "/no/such/file.json" })).is_err());
	}

	#[test]
	fn env_vars_are_expanded() {
		let env = |k: &str| (k == "T").then(|| "tok".to_owned());
		assert_eq!(expand("Bearer ${T}", env), "Bearer tok");
		assert_eq!(expand("${MISSING:-def}/${MISSING}", env), "def/");
		assert_eq!(expand("no ${unclosed", env), "no ${unclosed");
		let server = ServerConfig {
			command: Some("srv".into()),
			cwd: Some("sub".into()),
			..Default::default()
		};
		assert_eq!(
			server.resolved(Some(Path::new("/w"))).cwd,
			Some(PathBuf::from("/w/sub"))
		);
		assert_eq!(
			ServerConfig::default().resolved(Some(Path::new("/w"))).cwd,
			Some(PathBuf::from("/w"))
		);
	}

	#[test]
	fn names_and_results_fit_the_model() {
		assert_eq!(tool_name("fs", "read_file"), "mcp__fs__read_file");
		assert_eq!(tool_name("my server", "a.b"), "mcp__my_server__a_b");
		assert_eq!(tool_name("s", &"x".repeat(100)).len(), MAX_TOOL_NAME);

		let out = to_output(&json!({ "content": [
			{ "type": "text", "text": "hello" },
			{ "type": "image", "data": "...", "mimeType": "image/png" },
			{ "type": "resource", "resource": { "uri": "file:///a", "text": "body" } },
			{ "type": "resource_link", "uri": "file:///b" },
		]}));
		assert_eq!(
			out.content,
			"hello\n[image: image/png]\nbody\n[resource_link: file:///b]"
		);
		assert!(!out.is_error);
		let out = to_output(&json!({ "content": [], "structuredContent": { "n": 1 }, "isError": true }));
		assert_eq!((out.content.as_str(), out.is_error), (r#"{"n":1}"#, true));
	}
}
