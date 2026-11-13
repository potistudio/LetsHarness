//! Client がプラグインを使うときの窓口 (`Host`) と、外部プラグインを `dyn Trait` に見せる Adapter。
//!
//! Client は `Host` に `kind` 名で頼むだけで、どのプラグインが提供しているか・
//! どう通信しているかは知らない。

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::external::{Connection, EventSink, NoEvents, PluginEntry};
use crate::{
	AgentBackend, AgentHandler, AgentOutcome, AgentRequest, ChatMessage, ChatOutput, Completion, Error, ModelInfo,
	Provider, Quota, Result, Tool, ToolCall, ToolOutput, ToolRegistry, ToolSpec,
};

pub struct Host {
	entries: Vec<Arc<PluginEntry>>,
}

impl Host {
	pub(crate) fn new(entries: Vec<Arc<PluginEntry>>) -> Self {
		Self { entries }
	}

	/// 見つかったプラグインの名前。
	pub fn plugin_names(&self) -> Vec<&str> {
		self.entries.iter().map(|e| e.manifest.name.as_str()).collect()
	}

	/// `kind` の Provider を作る (プラグインは初回に起動する)。
	pub async fn provider(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn Provider>> {
		let entry = self
			.entries
			.iter()
			.find(|e| e.manifest.contributes.provider.iter().any(|k| k == kind))
			.ok_or_else(|| unknown("provider", kind, self.provider_kinds()))?;
		let conn = entry.connection().await?;
		let created = conn
			.call("provider.create", json!({ "kind": kind, "cfg": cfg }))
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
		let created = conn.call("agent.create", json!({ "kind": kind, "cfg": cfg })).await?;
		let (instance, name) = created_instance(entry.name, &created)?;
		let asks_approval = created.get("asks_approval").and_then(Value::as_bool).unwrap_or(true);
		Ok(Arc::new(RemoteAgent {
			conn,
			instance,
			name,
			asks_approval,
		}))
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
	pub async fn tools(&self, cfg: &Value) -> Result<ToolRegistry> {
		let mut tools = ToolRegistry::new();
		for entry in self.entries.iter().filter(|e| !e.manifest.contributes.tool.is_empty()) {
			let conn = entry.connection().await?;
			for kind in &entry.manifest.contributes.tool {
				let created = conn.call("tool.create", json!({ "kind": kind, "cfg": cfg })).await?;
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

#[async_trait::async_trait]
impl EventSink for StreamSink<'_> {
	async fn on_event(&mut self, method: &str, params: Value) -> Option<Value> {
		let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
		match method {
			"stream.delta" => (self.on_delta)(text),
			"stream.thinking" => (self.on_thinking)(text),
			_ => {}
		}
		None
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
	asks_approval: bool,
}

/// エージェントの途中経過・承認の問い合わせを [`AgentHandler`] に渡す。
struct HandlerSink<'a> {
	handler: &'a mut dyn AgentHandler,
}

#[async_trait::async_trait]
impl EventSink for HandlerSink<'_> {
	async fn on_event(&mut self, method: &str, params: Value) -> Option<Value> {
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
			"agent.approve" => {
				let spec = serde_json::from_value::<ToolSpec>(params.get("spec").cloned().unwrap_or_default());
				let approved = match (call(), spec) {
					(Some(call), Ok(spec)) => self.handler.approve(&call, &spec).await,
					_ => false,
				};
				return Some(json!(approved));
			}
			_ => {}
		}
		None
	}
}

#[async_trait::async_trait]
impl AgentBackend for RemoteAgent {
	fn name(&self) -> &'static str {
		self.name
	}

	fn asks_approval(&self) -> bool {
		self.asks_approval
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let v = self
			.conn
			.call("agent.list_models", json!({ "instance": self.instance }))
			.await?;
		serde_json::from_value(v).map_err(|e| Error::plugin(self.conn.name(), e))
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn AgentHandler) -> Result<AgentOutcome> {
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
