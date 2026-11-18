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

use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::rpc::{Frame, WireError};
use crate::{
	AgentBackend, AgentRequest, ChatEvent, ChatRequest, Completion, Error, Event, Hook, ModelRequest, Observer,
	Provider, Result, RunHandler, Tool, ToolCall, ToolOutput, ToolSpec,
};

/// `params[key]` を型に直す。無ければ `null` として扱う (`Option` なら `None`)。
fn field<T: DeserializeOwned>(params: &Value, key: &str) -> Result<T> {
	serde_json::from_value(params.get(key).cloned().unwrap_or(Value::Null))
		.map_err(|e| Error::Config(format!("bad params `{key}`: {e}")))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value> {
	serde_json::to_value(v).map_err(|e| Error::plugin("kernel", e))
}

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

/// ループに割り込む [`Hook`] の供給元。
pub trait HookFactory: Send + Sync {
	/// manifest の `contributes.hook` と一致させる。
	fn name(&self) -> &'static str;

	/// `cfg` はループに渡る設定 (`-o key=value` と `workdir`)。割り込まないなら `points()` が空の Hook を返す。
	/// `tools` は同じループで使うツール。設定がこれと食い違うなら `Err(Config)` で起動を止める。
	fn create(&self, cfg: &Value, tools: &[ToolSpec]) -> Result<Arc<dyn Hook>>;
}

/// ループの出来事を見る [`Observer`] の供給元。
pub trait ObserverFactory: Send + Sync {
	/// manifest の `contributes.observer` と一致させる。
	fn name(&self) -> &'static str;

	/// `cfg` はループに渡る設定。見ないなら `None` (出来事は送られてこない)。
	fn create(&self, cfg: &Value) -> Result<Option<Arc<dyn Observer>>>;
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
	hooks: Vec<Arc<dyn HookFactory>>,
	observers: Vec<Arc<dyn ObserverFactory>>,
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

	pub fn hook(mut self, factory: impl HookFactory + 'static) -> Self {
		self.hooks.push(Arc::new(factory));
		self
	}

	pub fn observer(mut self, factory: impl ObserverFactory + 'static) -> Self {
		self.observers.push(Arc::new(factory));
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
			hooks: Mutex::new(HashMap::new()),
			observers: Mutex::new(HashMap::new()),
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
				// Kernel からの通知 (Observer への出来事)。届いた順に処理するので、この後のリクエストより先に終わる。
				(None, Some(method)) => inner.notified(&method, frame.params.unwrap_or(Value::Null)),
				_ => {}
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
	hooks: Mutex<HashMap<u64, Arc<dyn Hook>>>,
	observers: Mutex<HashMap<u64, Arc<dyn Observer>>>,
}

impl Inner {
	fn instance_id(&self) -> u64 {
		self.next_instance.fetch_add(1, Ordering::Relaxed)
	}

	fn notify(&self, method: &str, req: u64, mut params: Value) {
		params["req"] = req.into();
		let _ = self.out.send(Frame::notify(method, params));
	}

	fn notified(&self, method: &str, params: Value) {
		let result = match method {
			"observer.event" => self.observer(&params).and_then(|observer| {
				observer.on_event(&field::<Event>(&params, "event")?);
				Ok(())
			}),
			other => Err(Error::Unsupported(other.to_owned())),
		};
		if let Err(e) = result {
			eprintln!("warning: {method}: {e}");
		}
	}

	fn observer(&self, params: &Value) -> Result<Arc<dyn Observer>> {
		let instance: u64 = field(params, "instance")?;
		self.observers
			.lock()
			.unwrap()
			.get(&instance)
			.cloned()
			.ok_or_else(|| Error::Config(format!("unknown observer instance: {instance}")))
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
				match &m["provider.".len()..] {
					"list_models" => to_value(provider.list_models().await?),
					"quota" => to_value(provider.quota().await?),
					// 応答の差分は、呼び出しの最中に通知として流す。
					"chat" => {
						let model: String = field(&params, "model")?;
						let mut stream = provider.chat(&model, field::<ChatRequest>(&params, "request")?);
						while let Some(event) = stream.next().await {
							match event? {
								ChatEvent::TextDelta { text } => {
									self.notify("stream.delta", req, json!({ "text": text }))
								}
								ChatEvent::ThinkingDelta { text } => {
									self.notify("stream.thinking", req, json!({ "text": text }))
								}
								ChatEvent::Done { completion } => return to_value(completion),
							}
						}
						Err(Error::plugin("kernel", "応答が `done` の前に終わりました"))
					}
					other => Err(Error::Unsupported(format!("provider.{other}"))),
				}
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
			"hook.create" => {
				let kind: String = field(&params, "kind")?;
				let factory = self
					.server
					.hooks
					.iter()
					.find(|f| f.name() == kind)
					.ok_or_else(|| Error::Config(format!("unknown hook: {kind}")))?;
				let tools: Vec<ToolSpec> = field::<Option<Vec<ToolSpec>>>(&params, "tools")?.unwrap_or_default();
				let hook = factory.create(params.get("cfg").unwrap_or(&Value::Null), &tools)?;
				let (id, points) = (self.instance_id(), hook.points());
				self.hooks.lock().unwrap().insert(id, hook);
				Ok(json!({ "instance": id, "points": points }))
			}
			m if m.starts_with("hook.") => {
				let instance: u64 = field(&params, "instance")?;
				let hook = self
					.hooks
					.lock()
					.unwrap()
					.get(&instance)
					.cloned()
					.ok_or_else(|| Error::Config(format!("unknown hook instance: {instance}")))?;
				match &m["hook.".len()..] {
					"before_request" => {
						to_value(hook.before_request(field::<ModelRequest>(&params, "request")?).await?)
					}
					"on_tool_call" => to_value(hook.on_tool_call(field::<ToolCall>(&params, "call")?).await?),
					"after_response" => {
						to_value(hook.after_response(field::<Completion>(&params, "completion")?).await?)
					}
					"after_tool_call" => {
						let call = field::<ToolCall>(&params, "call")?;
						to_value(hook.after_tool_call(&call, field(&params, "output")?).await?)
					}
					other => Err(Error::Unsupported(format!("hook.{other}"))),
				}
			}
			"observer.create" => {
				let kind: String = field(&params, "kind")?;
				let factory = self
					.server
					.observers
					.iter()
					.find(|f| f.name() == kind)
					.ok_or_else(|| Error::Config(format!("unknown observer: {kind}")))?;
				let Some(observer) = factory.create(params.get("cfg").unwrap_or(&Value::Null))? else {
					return Ok(json!({ "instance": null }));
				};
				let id = self.instance_id();
				self.observers.lock().unwrap().insert(id, observer);
				Ok(json!({ "instance": id }))
			}
			"observer.flush" => {
				self.observer(&params)?.flush().await;
				Ok(json!({}))
			}
			other => Err(Error::Unsupported(other.to_owned())),
		}
	}
}

/// エージェントの途中経過を Kernel へ通知する。
struct RpcHandler {
	inner: Arc<Inner>,
	req: u64,
}

impl RunHandler for RpcHandler {
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
