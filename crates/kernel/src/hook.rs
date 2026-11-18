//! Hook 拡張点: ループ ([`run_loop`](crate::run_loop)) の途中に割り込み、
//! モデルへのリクエスト・ツール呼び出しとその結果・モデルの応答を書き換えたり止めたりする。
//!
//! ループ自体は変えずに振る舞いを足すための口。複数の Hook は登録順に直列でつながり、
//! 前の Hook の出力が次の Hook の入力になる。Hook が `Err` を返すとループはそこで止まる
//! (ポリシーを黙って素通りさせないため)。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, ChatRequest, Completion, Event, Observer, Result, ToolCall, ToolOutput, ToolSpec};

/// Hook が割り込める箇所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
	/// モデルを呼ぶ直前 ([`Hook::before_request`])。
	BeforeRequest,
	/// ツールを実行する直前 ([`Hook::on_tool_call`])。
	OnToolCall,
	/// モデルの応答を受け取った直後 ([`Hook::after_response`])。
	AfterResponse,
	/// ツールの結果をモデルに返す直前 ([`Hook::after_tool_call`])。
	AfterToolCall,
}

/// モデルへの1回分のリクエスト ([`ChatRequest`] に送り先のモデルを足したもの)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
	pub model: String,
	pub messages: Vec<ChatMessage>,
	pub tools: Vec<ToolSpec>,
	pub max_tokens: Option<u32>,
	/// Provider 固有の指定 ([`ChatRequest::extra`])。
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub extra: serde_json::Value,
}

impl ModelRequest {
	pub fn new(model: impl Into<String>, req: ChatRequest) -> Self {
		Self {
			model: model.into(),
			messages: req.messages,
			tools: req.tools,
			max_tokens: req.max_tokens,
			extra: req.extra,
		}
	}

	/// `(model, Provider に渡す依頼)`。
	pub fn into_parts(self) -> (String, ChatRequest) {
		let req = ChatRequest {
			messages: self.messages,
			tools: self.tools,
			max_tokens: self.max_tokens,
			extra: self.extra,
		};
		(self.model, req)
	}
}

/// [`Hook::on_tool_call`] の判断。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ToolDecision {
	/// `call` を (書き換えたうえで) 実行する。
	Run { call: ToolCall },
	/// 実行せず、`output` を結果としてモデルに返す。以降の Hook は呼ばれない。
	Skip { output: ToolOutput },
}

/// ループに割り込む Hook。
///
/// [`points`](Hook::points) に挙げた箇所だけが呼ばれる。それ以外のメソッドは既定で素通しする。
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
	/// 割り込む箇所。空なら一度も呼ばれない。
	fn points(&self) -> Vec<HookPoint>;

	/// モデルを呼ぶ直前。書き換えはその1回のリクエストにだけ効き、
	/// ループの履歴 ([`LoopOutcome::messages`](crate::LoopOutcome::messages)) には残らない。
	async fn before_request(&self, req: ModelRequest) -> Result<ModelRequest> {
		Ok(req)
	}

	/// ツールを実行する直前。`call.id` は書き換えても元に戻る (結果との対応を保つため)。
	async fn on_tool_call(&self, call: ToolCall) -> Result<ToolDecision> {
		Ok(ToolDecision::Run { call })
	}

	/// モデルの応答を受け取った直後。書き換えた応答が履歴に残り、ツール呼び出しもこれに従う。
	async fn after_response(&self, completion: Completion) -> Result<Completion> {
		Ok(completion)
	}

	/// ツールの結果をモデルに返す直前。`on_tool_call` で `Skip` された結果も来る。
	/// `call` は実際に扱った呼び出し (`on_tool_call` で書き換えた後のもの)。
	async fn after_tool_call(&self, _call: &ToolCall, output: ToolOutput) -> Result<ToolOutput> {
		Ok(output)
	}
}

/// Hook を登録順に保持し、箇所ごとに直列で適用する。
///
/// [`Observer`] も持ち、各箇所で Hook をすべて通した後の値を [`Event`] として渡す。
#[derive(Clone, Default)]
pub struct HookRegistry {
	hooks: Vec<(Vec<HookPoint>, Arc<dyn Hook>)>,
	observers: Vec<Arc<dyn Observer>>,
}

impl HookRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	/// 末尾に登録する。割り込む箇所はこのとき1度だけ読む。
	pub fn add(&mut self, hook: Arc<dyn Hook>) -> &mut Self {
		self.hooks.push((hook.points(), hook));
		self
	}

	pub fn with(mut self, hook: Arc<dyn Hook>) -> Self {
		self.add(hook);
		self
	}

	/// Observer を末尾に登録する。
	pub fn observe(&mut self, observer: Arc<dyn Observer>) -> &mut Self {
		self.observers.push(observer);
		self
	}

	/// Observer がそれまでの出来事を処理し終えるまで待つ。
	pub async fn flush(&self) {
		for observer in &self.observers {
			observer.flush().await;
		}
	}

	fn emit(&self, event: impl FnOnce() -> Event) {
		if self.observers.is_empty() {
			return;
		}
		let event = event();
		for observer in &self.observers {
			observer.on_event(&event);
		}
	}

	/// Hook の数 (Observer は含まない)。
	pub fn is_empty(&self) -> bool {
		self.hooks.is_empty()
	}

	pub fn len(&self) -> usize {
		self.hooks.len()
	}

	fn at(&self, point: HookPoint) -> impl Iterator<Item = &Arc<dyn Hook>> {
		self.hooks
			.iter()
			.filter(move |(points, _)| points.contains(&point))
			.map(|(_, hook)| hook)
	}

	/// [`Hook::before_request`] を順に適用する。
	pub async fn before_request(&self, mut req: ModelRequest) -> Result<ModelRequest> {
		for hook in self.at(HookPoint::BeforeRequest) {
			req = hook.before_request(req).await?;
		}
		self.emit(|| Event::Request { request: req.clone() });
		Ok(req)
	}

	/// [`Hook::on_tool_call`] を順に適用する。最初の [`ToolDecision::Skip`] で止まる。
	pub async fn on_tool_call(&self, mut call: ToolCall) -> Result<ToolDecision> {
		for hook in self.at(HookPoint::OnToolCall) {
			let id = call.id.clone();
			match hook.on_tool_call(call).await? {
				ToolDecision::Run { call: mut next } => {
					next.id = id;
					call = next;
				}
				skip @ ToolDecision::Skip { .. } => return Ok(skip),
			}
		}
		Ok(ToolDecision::Run { call })
	}

	/// [`Hook::after_response`] を順に適用する。
	pub async fn after_response(&self, mut completion: Completion) -> Result<Completion> {
		for hook in self.at(HookPoint::AfterResponse) {
			completion = hook.after_response(completion).await?;
		}
		self.emit(|| Event::Response {
			completion: completion.clone(),
		});
		Ok(completion)
	}

	/// [`Hook::after_tool_call`] を順に適用する。
	pub async fn after_tool_call(&self, call: &ToolCall, mut output: ToolOutput) -> Result<ToolOutput> {
		for hook in self.at(HookPoint::AfterToolCall) {
			output = hook.after_tool_call(call, output).await?;
		}
		self.emit(|| Event::ToolResult {
			call: call.clone(),
			output: output.clone(),
		});
		Ok(output)
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use serde_json::json;

	use super::*;

	/// 名前を記録し、ツール引数に自分の名前を足す Hook。
	struct Tag {
		name: &'static str,
		points: Vec<HookPoint>,
		seen: Arc<Mutex<Vec<&'static str>>>,
	}

	#[async_trait::async_trait]
	impl Hook for Tag {
		fn points(&self) -> Vec<HookPoint> {
			self.points.clone()
		}
		async fn on_tool_call(&self, mut call: ToolCall) -> Result<ToolDecision> {
			self.seen.lock().unwrap().push(self.name);
			call.arguments["tags"]
				.as_array_mut()
				.expect("tags")
				.push(json!(self.name));
			call.id = "rewritten".into();
			Ok(ToolDecision::Run { call })
		}
	}

	struct Block;

	#[async_trait::async_trait]
	impl Hook for Block {
		fn points(&self) -> Vec<HookPoint> {
			vec![HookPoint::OnToolCall]
		}
		async fn on_tool_call(&self, _call: ToolCall) -> Result<ToolDecision> {
			Ok(ToolDecision::Skip {
				output: ToolOutput::error("blocked"),
			})
		}
	}

	fn tag(name: &'static str, points: Vec<HookPoint>, seen: &Arc<Mutex<Vec<&'static str>>>) -> Arc<dyn Hook> {
		Arc::new(Tag {
			name,
			points,
			seen: seen.clone(),
		})
	}

	#[tokio::test]
	async fn hooks_chain_in_order_and_keep_the_call_id() {
		let seen = Arc::default();
		let hooks = HookRegistry::new()
			.with(tag("a", vec![HookPoint::OnToolCall], &seen))
			.with(tag("skipped", vec![HookPoint::BeforeRequest], &seen))
			.with(tag("b", vec![HookPoint::OnToolCall], &seen));
		let call = ToolCall::new("c1", "echo", json!({ "tags": [] }));
		let ToolDecision::Run { call } = hooks.on_tool_call(call).await.unwrap() else {
			panic!("should run");
		};
		assert_eq!(call.arguments["tags"], json!(["a", "b"]));
		assert_eq!(call.id, "c1");
		assert_eq!(*seen.lock().unwrap(), ["a", "b"]);
	}

	#[tokio::test]
	async fn skip_stops_the_chain() {
		let seen = Arc::default();
		let hooks = HookRegistry::new()
			.with(Arc::new(Block))
			.with(tag("after", vec![HookPoint::OnToolCall], &seen));
		let decision = hooks
			.on_tool_call(ToolCall::new("c1", "echo", json!({ "tags": [] })))
			.await
			.unwrap();
		assert!(matches!(decision, ToolDecision::Skip { output } if output.is_error && output.content == "blocked"));
		assert!(seen.lock().unwrap().is_empty());
	}

	#[derive(Default)]
	struct Rec(Mutex<Vec<Event>>);

	impl Observer for Rec {
		fn on_event(&self, event: &Event) {
			self.0.lock().unwrap().push(event.clone());
		}
	}

	#[tokio::test]
	async fn observers_see_values_after_every_hook() {
		let rec = Arc::new(Rec::default());
		let mut hooks = HookRegistry::new().with(Arc::new(Block));
		hooks.observe(rec.clone());
		let call = ToolCall::new("c1", "echo", json!({}));
		let ToolDecision::Skip { output } = hooks.on_tool_call(call.clone()).await.unwrap() else {
			panic!("should skip");
		};
		hooks.after_tool_call(&call, output).await.unwrap();
		let events = rec.0.lock().unwrap();
		assert_eq!(events.len(), 1);
		assert!(matches!(&events[0], Event::ToolResult { output, .. } if output.content == "blocked"));
		assert_eq!(hooks.len(), 1, "observers are not hooks");
	}

	#[test]
	fn decisions_cross_the_wire() {
		let skip = serde_json::to_value(ToolDecision::Skip {
			output: ToolOutput::ok("x"),
		})
		.unwrap();
		assert_eq!(
			skip,
			json!({ "decision": "skip", "output": { "content": "x", "is_error": false } })
		);
		assert_eq!(
			serde_json::to_value(HookPoint::BeforeRequest).unwrap(),
			json!("before_request")
		);
	}
}
