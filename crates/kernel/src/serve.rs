//! プラグイン側: 自分が提供するものを宣言して、Kernel からの呼び出しに答える。
//!
//! プラグインの `main` はこれだけ。
//!
//! ```ignore
//! #[tokio::main]
//! async fn main() {
//!     PluginServer::new().provider(MyFactory).serve().await;
//! }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::dispatch::{call_provider, field, to_value};
use crate::rpc::{Frame, WireError};
use crate::{
	AgentBackend, AgentHandler, AgentRequest, ChatMessage, ChatOutput, Completion, Error, HostApi, ModelInfo, Provider,
	Quota, Result, Tool, ToolCall, ToolOutput, ToolRegistry, ToolSpec,
};

// ---------------------------------------------------------------------------
// プラグインが実装する拡張点
// ---------------------------------------------------------------------------

/// [`Provider`] (モデル) の「種類」。`cfg` でインスタンスを作る。
pub trait ProviderFactory: Send + Sync {
	/// 例: `"commandcode"`。manifest の `contributes.provider` と一致させる。
	fn kind(&self) -> &'static str;

	/// `cfg` は JSON オブジェクト (`{}` なら既定)。
	fn create(&self, cfg: &Value) -> Result<Arc<dyn Provider>>;
}

/// 外部エージェント ([`AgentBackend`]) の「種類」。
pub trait AgentBackendFactory: Send + Sync {
	/// 例: `"antigravity"`。
	fn kind(&self) -> &'static str;

	/// `kind` の別名 (例: `"agy"`)。manifest の `contributes.agent` にも並べる。
	fn aliases(&self) -> &'static [&'static str] {
		&[]
	}

	/// `cfg.workdir` があれば作業ディレクトリ。
	fn create(&self, cfg: &Value) -> Result<Arc<dyn AgentBackend>>;
}

/// ツールの供給元。
pub trait ToolFactory: Send + Sync {
	/// manifest の `contributes.tool` と一致させる。
	fn name(&self) -> &'static str;

	/// `cfg.workdir` があれば作業ディレクトリ。
	fn create(&self, cfg: &Value) -> Result<Vec<Arc<dyn Tool>>>;
}

/// 起動時に呼ばれ、以降に起動するプラグインへ渡す環境変数を返す。
pub trait BootHook: Send + Sync {
	/// `options` は `-o key=value` を集めた JSON オブジェクト。
	fn boot(&self, options: &Value) -> Result<HashMap<String, String>>;
}

// ---------------------------------------------------------------------------
// サーバ
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct PluginServer {
	boot: Option<Arc<dyn BootHook>>,
	providers: Vec<Arc<dyn ProviderFactory>>,
	agents: Vec<Arc<dyn AgentBackendFactory>>,
	tools: Vec<Arc<dyn ToolFactory>>,
}

impl PluginServer {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn boot_hook(mut self, hook: impl BootHook + 'static) -> Self {
		self.boot = Some(Arc::new(hook));
		self
	}

	pub fn provider(mut self, factory: impl ProviderFactory + 'static) -> Self {
		self.providers.push(Arc::new(factory));
		self
	}

	pub fn agent(mut self, factory: impl AgentBackendFactory + 'static) -> Self {
		self.agents.push(Arc::new(factory));
		self
	}

	pub fn tool(mut self, factory: impl ToolFactory + 'static) -> Self {
		self.tools.push(Arc::new(factory));
		self
	}

	/// stdin が閉じるまで Kernel からの呼び出しに答える。
	pub async fn serve(self) {
		let (out, mut out_rx) = mpsc::unbounded_channel::<Frame>();
		let inner = Arc::new(Inner {
			server: self,
			out,
			pending: Mutex::new(HashMap::new()),
			next_out: AtomicU64::new(1),
			next_instance: AtomicU64::new(1),
			providers: Mutex::new(HashMap::new()),
			agents: Mutex::new(HashMap::new()),
			tools: Mutex::new(HashMap::new()),
		});

		let writer = tokio::spawn(async move {
			let mut stdout = tokio::io::stdout();
			while let Some(frame) = out_rx.recv().await {
				let Ok(mut line) = serde_json::to_string(&frame) else {
					continue;
				};
				line.push('\n');
				if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
					break;
				}
			}
		});

		let mut lines = BufReader::new(tokio::io::stdin()).lines();
		while let Ok(Some(line)) = lines.next_line().await {
			let Ok(frame) = serde_json::from_str::<Frame>(&line) else {
				continue;
			};
			match (frame.id, frame.method) {
				// Kernel からのリクエスト。長い呼び出しの最中も他を受けられるよう別タスクで処理する。
				(Some(id), Some(method)) => {
					let inner = inner.clone();
					let params = frame.params.unwrap_or(Value::Null);
					tokio::spawn(async move {
						let reply = match inner.dispatch(&method, params).await {
							Ok(v) => Frame::ok(id, v),
							Err(e) => Frame::err(id, WireError::from(&e)),
						};
						let _ = inner.out.send(reply);
					});
				}
				// こちらが出した問い合わせへの答え。
				(Some(id), None) => {
					if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
						let _ = tx.send(match frame.error {
							Some(e) => Err(e),
							None => Ok(frame.result.unwrap_or(Value::Null)),
						});
					}
				}
				(None, _) => {}
			}
		}
		drop(inner);
		let _ = writer.await;
	}
}

struct Inner {
	server: PluginServer,
	out: mpsc::UnboundedSender<Frame>,
	/// Kernel への問い合わせ (承認など) の答え待ち。
	pending: Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, WireError>>>>,
	next_out: AtomicU64,
	next_instance: AtomicU64,
	providers: Mutex<HashMap<u64, Arc<dyn Provider>>>,
	agents: Mutex<HashMap<u64, Arc<dyn AgentBackend>>>,
	tools: Mutex<HashMap<u64, Arc<dyn Tool>>>,
}

impl Inner {
	fn instance_id(&self) -> u64 {
		self.next_instance.fetch_add(1, Ordering::Relaxed)
	}

	fn notify(&self, method: &str, req: u64, mut params: Value) {
		params["req"] = req.into();
		let _ = self.out.send(Frame::notify(method, params));
	}

	/// Kernel に問い合わせて答えを待つ。`req` は処理中の元リクエストの id (kernel はそれで受け手を決める)。
	async fn ask(&self, method: &str, req: u64, mut params: Value) -> Result<Value> {
		params["req"] = req.into();
		let id = self.next_out.fetch_add(1, Ordering::Relaxed);
		let (tx, rx) = oneshot::channel();
		self.pending.lock().unwrap().insert(id, tx);
		if self.out.send(Frame::request(id, method, params)).is_err() {
			return Err(Error::plugin("kernel", "connection closed"));
		}
		match rx.await {
			Ok(Ok(v)) => Ok(v),
			Ok(Err(e)) => Err(e.into_error("kernel")),
			Err(_) => Err(Error::plugin("kernel", "connection closed")),
		}
	}

	async fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
		let req = params.get("req").and_then(Value::as_u64).unwrap_or(0);
		match method {
			"boot" => {
				let env = match &self.server.boot {
					Some(hook) => hook.boot(params.get("options").unwrap_or(&Value::Null))?,
					None => HashMap::new(),
				};
				Ok(json!({ "env": env }))
			}
			"provider.create" => {
				let kind: String = field(&params, "kind")?;
				let factory = self
					.server
					.providers
					.iter()
					.find(|f| f.kind() == kind)
					.ok_or_else(|| Error::Config(format!("unknown provider: {kind}")))?;
				let provider = factory.create(params.get("cfg").unwrap_or(&Value::Null))?;
				let (id, name) = (self.instance_id(), provider.name());
				self.providers.lock().unwrap().insert(id, provider);
				Ok(json!({ "instance": id, "name": name }))
			}
			m if m.starts_with("provider.") => {
				let instance: u64 = field(&params, "instance")?;
				let provider = self
					.providers
					.lock()
					.unwrap()
					.get(&instance)
					.cloned()
					.ok_or_else(|| Error::Config(format!("unknown provider instance: {instance}")))?;
				let method = &m["provider.".len()..];
				// ストリームの差分は、呼び出しの最中に通知として流す。
				let mut delta = |text: &str| self.notify("stream.delta", req, json!({ "text": text }));
				let mut thinking = |text: &str| self.notify("stream.thinking", req, json!({ "text": text }));
				call_provider(&*provider, method, &params, &mut delta, &mut thinking).await
			}
			"agent.create" => {
				let kind: String = field(&params, "kind")?;
				let factory = self
					.server
					.agents
					.iter()
					.find(|f| f.kind() == kind || f.aliases().contains(&kind.as_str()))
					.ok_or_else(|| Error::Config(format!("unknown backend: {kind}")))?;
				let agent = factory.create(params.get("cfg").unwrap_or(&Value::Null))?;
				let (id, name, asks) = (self.instance_id(), agent.name(), agent.asks_approval());
				self.agents.lock().unwrap().insert(id, agent);
				Ok(json!({ "instance": id, "name": name, "asks_approval": asks }))
			}
			"agent.list_models" | "agent.run" => {
				let instance: u64 = field(&params, "instance")?;
				let agent = self
					.agents
					.lock()
					.unwrap()
					.get(&instance)
					.cloned()
					.ok_or_else(|| Error::Config(format!("unknown agent instance: {instance}")))?;
				if method == "agent.list_models" {
					return to_value(agent.list_models().await?);
				}
				let request: AgentRequest = field(&params, "request")?;
				let mut handler = RpcHandler {
					inner: self.clone(),
					req,
					link: KernelLink {
						inner: self.clone(),
						req,
					},
				};
				to_value(agent.run(request, &mut handler).await?)
			}
			"tool.create" => {
				let kind: String = field(&params, "kind")?;
				let factory = self
					.server
					.tools
					.iter()
					.find(|f| f.name() == kind)
					.ok_or_else(|| Error::Config(format!("unknown tool source: {kind}")))?;
				let mut infos = Vec::new();
				for tool in factory.create(params.get("cfg").unwrap_or(&Value::Null))? {
					let id = self.instance_id();
					infos.push(json!({ "id": id, "spec": tool.spec() }));
					self.tools.lock().unwrap().insert(id, tool);
				}
				Ok(json!({ "tools": infos }))
			}
			"tool.call" => {
				let id: u64 = field(&params, "tool")?;
				let tool = self
					.tools
					.lock()
					.unwrap()
					.get(&id)
					.cloned()
					.ok_or_else(|| Error::Config(format!("unknown tool: {id}")))?;
				to_value(tool.call(params.get("args").cloned().unwrap_or(Value::Null)).await)
			}
			other => Err(Error::Unsupported(other.to_owned())),
		}
	}
}

/// エージェントの途中経過を Kernel へ通知し、承認とモデル・ツールの借用は Kernel に問い合わせる。
struct RpcHandler {
	inner: Arc<Inner>,
	req: u64,
	link: KernelLink,
}

#[async_trait::async_trait]
impl AgentHandler for RpcHandler {
	fn on_text(&mut self, text: &str) {
		self.inner.notify("agent.on_text", self.req, json!({ "text": text }));
	}

	fn on_thinking(&mut self, thinking: &str) {
		self.inner
			.notify("agent.on_thinking", self.req, json!({ "text": thinking }));
	}

	fn on_tool_call(&mut self, call: &ToolCall) {
		self.inner
			.notify("agent.on_tool_call", self.req, json!({ "call": call }));
	}

	fn on_tool_result(&mut self, call: &ToolCall, output: &ToolOutput) {
		self.inner.notify(
			"agent.on_tool_result",
			self.req,
			json!({ "call": call, "output": output }),
		);
	}

	async fn approve(&mut self, call: &ToolCall, spec: &ToolSpec) -> bool {
		let answer = self
			.inner
			.ask("agent.approve", self.req, json!({ "call": call, "spec": spec }))
			.await;
		answer.ok().and_then(|v| v.as_bool()).unwrap_or(false)
	}

	fn host(&self) -> Option<&dyn HostApi> {
		Some(&self.link)
	}
}

// ---------------------------------------------------------------------------
// Kernel から借りる Provider / Tool
// ---------------------------------------------------------------------------

/// 実行中の `agent.run` の中から、kernel に頼む窓口。
#[derive(Clone)]
struct KernelLink {
	inner: Arc<Inner>,
	req: u64,
}

#[async_trait::async_trait]
impl HostApi for KernelLink {
	async fn provider(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn Provider>> {
		let created = self
			.inner
			.ask("host.provider.create", self.req, json!({ "kind": kind, "cfg": cfg }))
			.await?;
		let instance: u64 = field(&created, "instance")?;
		let name = created.get("name").and_then(Value::as_str).unwrap_or(kind);
		Ok(Arc::new(KernelProvider {
			link: self.clone(),
			instance,
			name: Box::leak(name.to_owned().into_boxed_str()),
		}))
	}

	async fn tools(&self, cfg: &Value) -> Result<ToolRegistry> {
		let created = self
			.inner
			.ask("host.tools.create", self.req, json!({ "cfg": cfg }))
			.await?;
		let list: Vec<KernelToolInfo> = field(&created, "tools")?;
		let mut tools = ToolRegistry::new();
		for info in list {
			tools.add(Arc::new(KernelTool {
				link: self.clone(),
				id: info.id,
				spec: info.spec,
			}));
		}
		Ok(tools)
	}
}

#[derive(serde::Deserialize)]
struct KernelToolInfo {
	id: u64,
	spec: ToolSpec,
}

/// kernel が持つ Provider を線越しに使う。エージェントループが使う非ストリームの呼び出しだけを通し、
/// ストリームは `Unsupported`。
struct KernelProvider {
	link: KernelLink,
	instance: u64,
	name: &'static str,
}

impl KernelProvider {
	async fn rpc<R: DeserializeOwned>(&self, method: &str, mut params: Value) -> Result<R> {
		params["instance"] = self.instance.into();
		params["method"] = method.into();
		let v = self.link.inner.ask("host.provider.call", self.link.req, params).await?;
		serde_json::from_value(v).map_err(|e| Error::plugin("kernel", e))
	}
}

#[async_trait::async_trait]
impl Provider for KernelProvider {
	fn name(&self) -> &'static str {
		self.name
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		self.rpc("list_models", json!({})).await
	}

	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, max_tokens: Option<u32>) -> Result<String> {
		self.rpc(
			"chat",
			json!({ "model": model, "messages": messages, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn chat_stream(
		&self,
		_model: &str,
		_messages: Vec<ChatMessage>,
		_on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		_on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		Err(Error::Unsupported("chat_stream (kernel 経由の借用では非対応)".into()))
	}

	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		self.rpc("responses", json!({ "model": model, "input": input })).await
	}

	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<String> {
		self.rpc(
			"messages",
			json!({ "model": model, "messages": messages, "system": system, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn quota(&self) -> Result<Quota> {
		self.rpc("quota", json!({})).await
	}

	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		self.rpc(
			"chat_with_usage",
			json!({ "model": model, "messages": messages, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn chat_stream_with_usage(
		&self,
		_model: &str,
		_messages: Vec<ChatMessage>,
		_on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		_on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		Err(Error::Unsupported(
			"chat_stream_with_usage (kernel 経由の借用では非対応)".into(),
		))
	}

	async fn chat_with_tools(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		self.rpc(
			"chat_with_tools",
			json!({ "model": model, "messages": messages, "tools": tools, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		self.rpc("responses_with_usage", json!({ "model": model, "input": input }))
			.await
	}

	async fn messages_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<ChatOutput> {
		self.rpc(
			"messages_with_usage",
			json!({ "model": model, "messages": messages, "system": system, "max_tokens": max_tokens }),
		)
		.await
	}
}

/// kernel が持つツールを線越しに使う。
struct KernelTool {
	link: KernelLink,
	id: u64,
	spec: ToolSpec,
}

#[async_trait::async_trait]
impl Tool for KernelTool {
	fn spec(&self) -> ToolSpec {
		self.spec.clone()
	}

	async fn call(&self, args: Value) -> ToolOutput {
		let answer = self
			.link
			.inner
			.ask(
				"host.tool.call",
				self.link.req,
				json!({ "tool": self.id, "args": args }),
			)
			.await;
		match answer.and_then(|v| serde_json::from_value(v).map_err(|e| Error::plugin("kernel", e))) {
			Ok(output) => output,
			Err(e) => ToolOutput::error(e.to_string()),
		}
	}
}
