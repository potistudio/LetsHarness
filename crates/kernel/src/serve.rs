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
use tokio::sync::mpsc;

use crate::ToolSpec;
use crate::rpc::{Frame, WireError};
use crate::{
	AgentBackend, AgentHandler, AgentRequest, ChatMessage, Error, Provider, Result, Tool, ToolCall, ToolOutput,
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
			// Kernel からのリクエスト。長い呼び出しの最中も他を受けられるよう別タスクで処理する。
			if let (Some(id), Some(method)) = (frame.id, frame.method) {
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
		}
		drop(inner);
		let _ = writer.await;
	}
}

struct Inner {
	server: PluginServer,
	out: mpsc::UnboundedSender<Frame>,
	next_instance: AtomicU64,
	providers: Mutex<HashMap<u64, Arc<dyn Provider>>>,
	agents: Mutex<HashMap<u64, Arc<dyn AgentBackend>>>,
	tools: Mutex<HashMap<u64, Arc<dyn Tool>>>,
}

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
	Error::Config(format!("bad params `{what}`: {e}"))
}

/// `params[key]` を型に直す。無ければ `null` として扱う (`Option` なら `None`)。
fn field<T: DeserializeOwned>(params: &Value, key: &str) -> Result<T> {
	serde_json::from_value(params.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| bad(key, e))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value> {
	serde_json::to_value(v).map_err(|e| Error::plugin("serve", e))
}

impl Inner {
	fn instance_id(&self) -> u64 {
		self.next_instance.fetch_add(1, Ordering::Relaxed)
	}

	fn notify(&self, method: &str, req: u64, mut params: Value) {
		params["req"] = req.into();
		let _ = self.out.send(Frame::notify(method, params));
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
				self.provider_call(&*provider, &m["provider.".len()..], req, &params)
					.await
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
				let (id, name) = (self.instance_id(), agent.name());
				self.agents.lock().unwrap().insert(id, agent);
				Ok(json!({ "instance": id, "name": name }))
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

	async fn provider_call(&self, p: &dyn Provider, method: &str, req: u64, params: &Value) -> Result<Value> {
		let model = || field::<String>(params, "model");
		let messages = || field::<Vec<ChatMessage>>(params, "messages");
		let max_tokens = || field::<Option<u32>>(params, "max_tokens");
		let system = || field::<Option<String>>(params, "system");

		// ストリームの差分は、呼び出しの最中に通知として流す。
		let delta = |text: &str| self.notify("stream.delta", req, json!({ "text": text }));
		let thinking = |text: &str| self.notify("stream.thinking", req, json!({ "text": text }));

		match method {
			"list_models" => to_value(p.list_models().await?),
			"chat" => to_value(p.chat(&model()?, messages()?, max_tokens()?).await?),
			"chat_stream" => {
				let (mut d, mut t) = (delta, thinking);
				to_value(p.chat_stream(&model()?, messages()?, &mut d, &mut t).await?)
			}
			"responses" => to_value(p.responses(&model()?, &field::<String>(params, "input")?).await?),
			"messages" => {
				let max = field::<u32>(params, "max_tokens")?;
				to_value(p.messages(&model()?, messages()?, system()?.as_deref(), max).await?)
			}
			"quota" => to_value(p.quota().await?),
			"chat_with_usage" => to_value(p.chat_with_usage(&model()?, messages()?, max_tokens()?).await?),
			"chat_stream_with_usage" => {
				let (mut d, mut t) = (delta, thinking);
				to_value(p.chat_stream_with_usage(&model()?, messages()?, &mut d, &mut t).await?)
			}
			"chat_with_tools" => {
				let tools = field::<Vec<ToolSpec>>(params, "tools")?;
				to_value(p.chat_with_tools(&model()?, messages()?, &tools, max_tokens()?).await?)
			}
			"responses_with_usage" => to_value(
				p.responses_with_usage(&model()?, &field::<String>(params, "input")?)
					.await?,
			),
			"messages_with_usage" => {
				let max = field::<u32>(params, "max_tokens")?;
				to_value(
					p.messages_with_usage(&model()?, messages()?, system()?.as_deref(), max)
						.await?,
				)
			}
			other => Err(Error::Unsupported(format!("provider.{other}"))),
		}
	}
}

/// エージェントの途中経過を Kernel へ通知する。
struct RpcHandler {
	inner: Arc<Inner>,
	req: u64,
}

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
}
