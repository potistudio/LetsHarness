//! Hook 拡張点: ループ ([`run_loop`](crate::run_loop)) の途中に割り込み、
//! モデルへのリクエスト・ツール呼び出し・モデルの応答を書き換えたり止めたりする。
//!
//! ループ自体は変えずに振る舞いを足すための口。複数の Hook は登録順に直列でつながり、
//! 前の Hook の出力が次の Hook の入力になる。Hook が `Err` を返すとループはそこで止まる
//! (ポリシーを黙って素通りさせないため)。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, Completion, Result, ToolCall, ToolOutput, ToolSpec};

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
}

/// モデルへの1回分のリクエスト。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
	pub model: String,
	pub messages: Vec<ChatMessage>,
	pub tools: Vec<ToolSpec>,
	pub max_tokens: Option<u32>,
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
}

/// Hook を登録順に保持し、箇所ごとに直列で適用する。
#[derive(Clone, Default)]
pub struct HookRegistry {
	hooks: Vec<(Vec<HookPoint>, Arc<dyn Hook>)>,
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
		Ok(completion)
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
