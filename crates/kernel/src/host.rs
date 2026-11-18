//! Client がプラグインを使うときの窓口 (`Host`) と、外部プラグインを `dyn Trait` に見せる Adapter。
//!
//! Client は `Host` に `kind` 名で頼むだけで、どのプラグインが提供しているか・
//! どう通信しているかは知らない。

use std::path::PathBuf;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::external::{Connection, EventSink, NoEvents, PluginEntry};
use crate::session_log::{self, SessionId, log_path};
use crate::{
	AgentBackend, AgentOutcome, AgentRequest, ChatMessage, ChatOutput, Completion, Error, Event, Hook, HookPoint,
	HookRegistry, ModelInfo, ModelRequest, Observer, Provider, Quota, Result, RunHandler, Session, Tool, ToolCall,
	ToolDecision, ToolOutput, ToolRegistry, ToolSpec,
};

pub struct Host {
	entries: Arc<Vec<Arc<PluginEntry>>>,
	sessions_dir: Option<PathBuf>,
}

impl Host {
	pub(crate) fn new(entries: Vec<Arc<PluginEntry>>, sessions_dir: Option<PathBuf>) -> Self {
		Self {
			entries: Arc::new(entries),
			sessions_dir,
		}
	}

	/// 見つかったプラグインの名前。
	pub fn plugin_names(&self) -> Vec<&str> {
		self.entries.iter().map(|e| e.manifest.name.as_str()).collect()
	}

	/// `kind` の Provider と、すべてのツール・Hook・Observer をまとめた [`Session`] を作る。
	///
	/// Client がモデルを使う窓口はこれだけ。ツールより後に Hook を作るので、Hook はツールを見て設定を検証できる。
	///
	/// 会話はセッションログ (`<sessions_dir>/<id>.jsonl`) に残り、[`resume`](Self::resume) で再開できる。
	pub async fn session(&self, provider: &str, cfg: &Value) -> Result<Session> {
		let (p, tools, hooks) = self.parts(provider, cfg).await?;
		Ok(Session::create(provider, p, tools, hooks, self.sessions_dir.clone()))
	}

	/// セッションログから会話を復元し、続きから再開する。Provider はログに残った kind を使い、`cfg` は今回のもの。
	pub async fn resume(&self, id: &SessionId, cfg: &Value) -> Result<Session> {
		let dir = self
			.sessions_dir
			.clone()
			.ok_or_else(|| Error::Config("セッションの保存先がありません ($LETSHARNESS_HOME / $HOME)".into()))?;
		let (kind, history) = session_log::read(&log_path(&dir, id), id)?;
		let (p, tools, hooks) = self.parts(&kind, cfg).await?;
		Ok(Session::resumed(p, &kind, tools, hooks, dir, id.clone(), history))
	}

	async fn parts(&self, provider: &str, cfg: &Value) -> Result<(Arc<dyn Provider>, ToolRegistry, HookRegistry)> {
		let provider = self.provider(provider, cfg).await?;
		let tools = self.tools(cfg).await?;
		let mut hooks = self.hooks(cfg, &tools.specs()).await?;
		for observer in self.observers(cfg).await? {
			hooks.observe(observer);
		}
		Ok((provider, tools, hooks))
	}

	/// `kind` の Provider を作る (プラグインは初回に起動する)。
	pub(crate) async fn provider(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn Provider>> {
		let entry = self
			.entries
			.iter()
			.find(|e| e.manifest.contributes.provider.iter().any(|k| k == kind))
			.ok_or_else(|| unknown("provider", kind, self.provider_kinds()))?;
		let conn = entry.connection().await?;
		let created = conn
			.call("provider.create", json!({ "kind": kind, "cfg": entry.cfg(cfg) }))
			.await?;
		let (instance, name) = created_instance(entry.name, &created)?;
		Ok(Arc::new(RemoteProvider { conn, instance, name }))
	}

	pub fn provider_kinds(&self) -> Vec<&str> {
		self.kinds(|c| &c.provider)
	}

	/// `kind` (または別名) の外部エージェントを作る。
	pub async fn agent_backend(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn AgentBackend>> {
		let entry = self
			.entries
			.iter()
			.find(|e| e.manifest.contributes.agent.iter().any(|k| k == kind))
			.ok_or_else(|| unknown("backend", kind, self.agent_backend_kinds()))?;
		let conn = entry.connection().await?;
		let created = conn
			.call("agent.create", json!({ "kind": kind, "cfg": entry.cfg(cfg) }))
			.await?;
		let (instance, name) = created_instance(entry.name, &created)?;
		Ok(Arc::new(RemoteAgent { conn, instance, name }))
	}

	pub fn has_agent_backend(&self, kind: &str) -> bool {
		self.entries
			.iter()
			.any(|e| e.manifest.contributes.agent.iter().any(|k| k == kind))
	}

	pub fn agent_backend_kinds(&self) -> Vec<&str> {
		self.kinds(|c| &c.agent)
	}

	/// ツールを供給するすべてのプラグインからツールを集める。
	pub(crate) async fn tools(&self, cfg: &Value) -> Result<ToolRegistry> {
		let mut tools = ToolRegistry::new();
		for entry in self.entries.iter().filter(|e| !e.manifest.contributes.tool.is_empty()) {
			let conn = entry.connection().await?;
			for kind in &entry.manifest.contributes.tool {
				let created = conn
					.call("tool.create", json!({ "kind": kind, "cfg": entry.cfg(cfg) }))
					.await?;
				let list = created.get("tools").cloned().unwrap_or(Value::Null);
				let list: Vec<RemoteToolInfo> =
					serde_json::from_value(list).map_err(|e| Error::plugin(entry.name, e))?;
				for info in list {
					tools.add(Arc::new(RemoteTool {
						conn: conn.clone(),
						id: info.id,
						spec: info.spec,
					}));
				}
			}
		}
		Ok(tools)
	}

	/// Hook を提供するすべてのプラグインから Hook を集める。並びはプラグインの発見順 → manifest の記載順。
	///
	/// `tools` は同じループで使うツール。Hook はこれを見て設定を検証できる (存在しないツール名など)。
	pub(crate) async fn hooks(&self, cfg: &Value, tools: &[ToolSpec]) -> Result<HookRegistry> {
		let mut hooks = HookRegistry::new();
		for entry in self.entries.iter().filter(|e| !e.manifest.contributes.hook.is_empty()) {
			let conn = entry.connection().await?;
			for kind in &entry.manifest.contributes.hook {
				let created = conn
					.call(
						"hook.create",
						json!({ "kind": kind, "cfg": entry.cfg(cfg), "tools": tools }),
					)
					.await?;
				let instance = created
					.get("instance")
					.and_then(Value::as_u64)
					.ok_or_else(|| Error::plugin(entry.name, "hook.create: `instance` がありません"))?;
				let points: Vec<HookPoint> =
					serde_json::from_value(created.get("points").cloned().unwrap_or(Value::Null))
						.map_err(|e| Error::plugin(entry.name, e))?;
				hooks.add(Arc::new(RemoteHook {
					conn: conn.clone(),
					instance,
					points,
				}));
			}
		}
		Ok(hooks)
	}

	pub fn hook_kinds(&self) -> Vec<&str> {
		self.kinds(|c| &c.hook)
	}

	/// Observer を提供するすべてのプラグインから Observer を集める。設定次第で見ないものは含まない。
	pub(crate) async fn observers(&self, cfg: &Value) -> Result<Vec<Arc<dyn Observer>>> {
		let mut observers: Vec<Arc<dyn Observer>> = Vec::new();
		for entry in self
			.entries
			.iter()
			.filter(|e| !e.manifest.contributes.observer.is_empty())
		{
			let conn = entry.connection().await?;
			for kind in &entry.manifest.contributes.observer {
				let created = conn
					.call("observer.create", json!({ "kind": kind, "cfg": entry.cfg(cfg) }))
					.await?;
				if let Some(instance) = created.get("instance").and_then(Value::as_u64) {
					observers.push(Arc::new(RemoteObserver::spawn(conn.clone(), instance)));
				}
			}
		}
		Ok(observers)
	}

	fn kinds<'a>(&'a self, pick: impl Fn(&'a crate::external::Contributes) -> &'a Vec<String>) -> Vec<&'a str> {
		self.entries
			.iter()
			.flat_map(|e| pick(&e.manifest.contributes).iter().map(String::as_str))
			.collect()
	}
}

fn unknown(what: &str, kind: &str, known: Vec<&str>) -> Error {
	Error::Config(format!("unknown {what}: {kind} ({})", known.join("|")))
}

/// `*.create` の応答から `(instance, name)` を取る。`name` は `Provider::name` が `&'static str` なので確保する。
fn created_instance(plugin: &'static str, created: &Value) -> Result<(u64, &'static str)> {
	let instance = created
		.get("instance")
		.and_then(Value::as_u64)
		.ok_or_else(|| Error::plugin(plugin, "create: `instance` がありません"))?;
	let name = created.get("name").and_then(Value::as_str).unwrap_or(plugin);
	Ok((instance, Box::leak(name.to_owned().into_boxed_str())))
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

struct RemoteProvider {
	conn: Arc<Connection>,
	instance: u64,
	name: &'static str,
}

/// ストリーム通知 (`stream.delta` / `stream.thinking`) をコールバックへ流す。
struct StreamSink<'a> {
	on_delta: &'a mut (dyn for<'x> FnMut(&'x str) + Send),
	on_thinking: &'a mut (dyn for<'x> FnMut(&'x str) + Send),
}

impl EventSink for StreamSink<'_> {
	fn on_event(&mut self, method: &str, params: Value) {
		let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
		match method {
			"stream.delta" => (self.on_delta)(text),
			"stream.thinking" => (self.on_thinking)(text),
			_ => {}
		}
	}
}

impl RemoteProvider {
	async fn call(&self, method: &str, mut params: Value, sink: &mut dyn EventSink) -> Result<Value> {
		params["instance"] = self.instance.into();
		self.conn.start(method, params).await?.drive(sink).await
	}

	async fn rpc<R: DeserializeOwned>(&self, method: &str, params: Value) -> Result<R> {
		let v = self.call(method, params, &mut NoEvents).await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}

	async fn stream<R: DeserializeOwned>(
		&self,
		method: &str,
		params: Value,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<R> {
		let mut sink = StreamSink { on_delta, on_thinking };
		let v = self.call(method, params, &mut sink).await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}
}

#[async_trait::async_trait]
impl Provider for RemoteProvider {
	fn name(&self) -> &'static str {
		self.name
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		self.rpc("provider.list_models", json!({})).await
	}

	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, max_tokens: Option<u32>) -> Result<String> {
		self.rpc(
			"provider.chat",
			json!({ "model": model, "messages": messages, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		self.stream(
			"provider.chat_stream",
			json!({ "model": model, "messages": messages }),
			on_delta,
			on_thinking,
		)
		.await
	}

	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		self.rpc("provider.responses", json!({ "model": model, "input": input }))
			.await
	}

	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<String> {
		self.rpc(
			"provider.messages",
			json!({ "model": model, "messages": messages, "system": system, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn quota(&self) -> Result<Quota> {
		self.rpc("provider.quota", json!({})).await
	}

	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		self.rpc(
			"provider.chat_with_usage",
			json!({ "model": model, "messages": messages, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		self.stream(
			"provider.chat_stream_with_usage",
			json!({ "model": model, "messages": messages }),
			on_delta,
			on_thinking,
		)
		.await
	}

	async fn chat_with_tools(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		self.rpc(
			"provider.chat_with_tools",
			json!({ "model": model, "messages": messages, "tools": tools, "max_tokens": max_tokens }),
		)
		.await
	}

	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		self.rpc(
			"provider.responses_with_usage",
			json!({ "model": model, "input": input }),
		)
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
			"provider.messages_with_usage",
			json!({ "model": model, "messages": messages, "system": system, "max_tokens": max_tokens }),
		)
		.await
	}
}

// ---------------------------------------------------------------------------
// AgentBackend
// ---------------------------------------------------------------------------

struct RemoteAgent {
	conn: Arc<Connection>,
	instance: u64,
	name: &'static str,
}

/// エージェントの途中経過を [`RunHandler`] に渡す。
struct HandlerSink<'a> {
	handler: &'a mut dyn RunHandler,
}

impl EventSink for HandlerSink<'_> {
	fn on_event(&mut self, method: &str, params: Value) {
		let text = || {
			params
				.get("text")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_owned()
		};
		let call = || serde_json::from_value::<ToolCall>(params.get("call").cloned().unwrap_or_default()).ok();
		match method {
			"agent.on_text" => self.handler.on_text(&text()),
			"agent.on_thinking" => self.handler.on_thinking(&text()),
			"agent.on_tool_call" => {
				if let Some(call) = call() {
					self.handler.on_tool_call(&call);
				}
			}
			"agent.on_tool_result" => {
				let output = serde_json::from_value::<ToolOutput>(params.get("output").cloned().unwrap_or_default());
				if let (Some(call), Ok(output)) = (call(), output) {
					self.handler.on_tool_result(&call, &output);
				}
			}
			_ => {}
		}
	}
}

#[async_trait::async_trait]
impl AgentBackend for RemoteAgent {
	fn name(&self) -> &'static str {
		self.name
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let v = self
			.conn
			.call("agent.list_models", json!({ "instance": self.instance }))
			.await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn RunHandler) -> Result<AgentOutcome> {
		let params = json!({ "instance": self.instance, "request": req });
		let mut sink = HandlerSink { handler };
		let v = self.conn.start("agent.run", params).await?.drive(&mut sink).await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RemoteToolInfo {
	id: u64,
	spec: ToolSpec,
}

struct RemoteTool {
	conn: Arc<Connection>,
	id: u64,
	spec: ToolSpec,
}

#[async_trait::async_trait]
impl Tool for RemoteTool {
	fn spec(&self) -> ToolSpec {
		self.spec.clone()
	}

	async fn call(&self, args: Value) -> ToolOutput {
		let result = self
			.conn
			.call("tool.call", json!({ "tool": self.id, "args": args }))
			.await;
		match result.and_then(|v| serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))) {
			Ok(output) => output,
			// 通信の失敗もハーネスを止めず、モデルに見せる (ツールの失敗と同じ扱い)。
			Err(e) => ToolOutput::error(e.to_string()),
		}
	}
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

struct RemoteHook {
	conn: Arc<Connection>,
	instance: u64,
	points: Vec<HookPoint>,
}

impl RemoteHook {
	async fn rpc<R: DeserializeOwned>(&self, method: &str, mut params: Value) -> Result<R> {
		params["instance"] = self.instance.into();
		let v = self.conn.call(method, params).await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}
}

/// 通信の失敗も `Err` として返し、ループを止める (ツールと違い、ポリシーを素通りさせないため)。
#[async_trait::async_trait]
impl Hook for RemoteHook {
	fn points(&self) -> Vec<HookPoint> {
		self.points.clone()
	}

	async fn before_request(&self, req: ModelRequest) -> Result<ModelRequest> {
		self.rpc("hook.before_request", json!({ "request": req })).await
	}

	async fn on_tool_call(&self, call: ToolCall) -> Result<ToolDecision> {
		self.rpc("hook.on_tool_call", json!({ "call": call })).await
	}

	async fn after_response(&self, completion: Completion) -> Result<Completion> {
		self.rpc("hook.after_response", json!({ "completion": completion }))
			.await
	}

	async fn after_tool_call(&self, call: &ToolCall, output: ToolOutput) -> Result<ToolOutput> {
		self.rpc("hook.after_tool_call", json!({ "call": call, "output": output }))
			.await
	}
}

// ---------------------------------------------------------------------------
// Observer
// ---------------------------------------------------------------------------

enum Outbox {
	/// 送る時点ではなく起きた時点の値を送るため、受け取ったときに JSON にしておく。
	Event(Value),
	Flush(oneshot::Sender<()>),
}

/// 出来事を順に通知として送る。ループは送信を待たない。
///
/// 送れなかった出来事は警告して捨てる (見るだけの口なので、ループは止めない)。
struct RemoteObserver {
	tx: mpsc::UnboundedSender<Outbox>,
}

impl RemoteObserver {
	fn spawn(conn: Arc<Connection>, instance: u64) -> Self {
		let (tx, mut rx) = mpsc::unbounded_channel();
		tokio::spawn(async move {
			while let Some(item) = rx.recv().await {
				let sent = match item {
					Outbox::Event(event) => {
						conn.notify("observer.event", json!({ "instance": instance, "event": event }))
							.await
					}
					Outbox::Flush(done) => {
						let flushed = conn.call("observer.flush", json!({ "instance": instance })).await;
						let _ = done.send(());
						flushed.map(drop)
					}
				};
				if let Err(e) = sent {
					eprintln!("warning: observer: {e}");
				}
			}
		});
		Self { tx }
	}
}

#[async_trait::async_trait]
impl Observer for RemoteObserver {
	fn on_event(&self, event: &Event) {
		match serde_json::to_value(event) {
			Ok(event) => {
				let _ = self.tx.send(Outbox::Event(event));
			}
			Err(e) => eprintln!("warning: observer: {e}"),
		}
	}

	async fn flush(&self) {
		let (done, wait) = oneshot::channel();
		if self.tx.send(Outbox::Flush(done)).is_ok() {
			let _ = wait.await;
		}
	}
}
