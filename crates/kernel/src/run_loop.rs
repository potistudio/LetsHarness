//! ループ本体。

use crate::{
	ApprovalPolicy, ChatMessage, DEFAULT_MAX_STEPS, Error, Provider, Result, RunHandler, ToolCall, ToolOutput,
	ToolRegistry, Usage,
};

/// ループの依頼。
#[derive(Debug, Clone)]
pub struct LoopRequest {
	pub model: String,
	pub messages: Vec<ChatMessage>,
	pub max_tokens: Option<u32>,
	/// モデルを呼ぶ回数の上限 (暴走防止)。
	pub max_steps: usize,
	pub approval: ApprovalPolicy,
}

impl LoopRequest {
	pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
		Self {
			model: model.into(),
			messages,
			max_tokens: None,
			max_steps: DEFAULT_MAX_STEPS,
			approval: ApprovalPolicy::default(),
		}
	}
}

#[derive(Debug, Clone)]
pub struct LoopOutcome {
	/// 最終ターンの本文。
	pub text: String,
	/// 入力 + 途中のツール往復 + 最終回答を含む全履歴 (会話の継続に使える)。
	pub messages: Vec<ChatMessage>,
	/// モデルを呼んだ回数。
	pub steps: usize,
	/// 全ステップ合算の使用量 (どのステップも usage を返さなければ `None`)。
	pub usage: Option<Usage>,
	/// `max_steps` に達して打ち切った (最終ターンがまだツールを要求していた)。
	/// このとき `messages` は未実行のツール要求を含まない。
	pub hit_step_limit: bool,
}

/// モデル呼び出し → ツール実行 → 結果を返して再呼び出し、を最終回答が出るまで繰り返す。
///
/// kernel の責務。モデルは [`Provider`]、ツールは [`ToolRegistry`] で、どこのプラグインのものかは知らない。
pub async fn run_loop(
	provider: &dyn Provider,
	tools: &ToolRegistry,
	handler: &mut dyn RunHandler,
	req: LoopRequest,
) -> Result<LoopOutcome> {
	let LoopRequest {
		model,
		mut messages,
		max_tokens,
		max_steps,
		approval,
	} = req;
	if max_steps == 0 {
		return Err(Error::Config("max_steps は 1 以上が必要です".into()));
	}
	let specs = tools.specs();
	let mut usage: Option<Usage> = None;

	for step in 1..=max_steps {
		let mut turn = provider
			.chat_with_tools(&model, messages.clone(), &specs, max_tokens)
			.await?;
		if let Some(u) = turn.usage.take() {
			merge_usage(&mut usage, u);
		}
		if let Some(t) = turn.thinking.as_deref().filter(|t| !t.trim().is_empty()) {
			handler.on_thinking(t);
		}

		if turn.tool_calls.is_empty() {
			messages.push(ChatMessage::assistant(turn.text.clone()));
			return Ok(LoopOutcome {
				text: turn.text,
				messages,
				steps: step,
				usage,
				hit_step_limit: false,
			});
		}
		if !turn.text.is_empty() {
			handler.on_text(&turn.text);
		}
		if step == max_steps {
			return Ok(LoopOutcome {
				text: turn.text,
				messages,
				steps: step,
				usage,
				hit_step_limit: true,
			});
		}

		// プロバイダがIDを付けなかった場合に備え、結果との対応が取れるよう補う。
		for (i, call) in turn.tool_calls.iter_mut().enumerate() {
			if call.id.is_empty() {
				call.id = format!("call_{step}_{i}");
			}
		}
		messages.push(ChatMessage::assistant_tool_calls(
			turn.text.clone(),
			turn.tool_calls.clone(),
		));
		for call in &turn.tool_calls {
			let output = execute(tools, handler, call, approval).await;
			handler.on_tool_result(call, &output);
			messages.push(ChatMessage::tool_result(&call.id, tool_result_text(&output)));
		}
	}
	unreachable!("max_steps >= 1 のためループ内で必ず return する")
}

async fn execute(
	tools: &ToolRegistry,
	handler: &mut dyn RunHandler,
	call: &ToolCall,
	approval: ApprovalPolicy,
) -> ToolOutput {
	handler.on_tool_call(call);
	let Some((spec, tool)) = tools.get(&call.name) else {
		return ToolOutput::error(format!("unknown tool: {}", call.name));
	};
	if spec.requires_approval && approval == ApprovalPolicy::Ask && !handler.approve(call, spec).await {
		return ToolOutput::error("denied: ユーザーがこのツール実行を許可しませんでした");
	}
	tool.call(call.arguments.clone()).await
}

/// モデルへ返す文字列。失敗は先頭の `Error:` で判別できるようにする
/// (メッセージ型は `is_error` を持たないため)。
fn tool_result_text(out: &ToolOutput) -> String {
	if out.is_error {
		format!("Error: {}", out.content)
	} else {
		out.content.clone()
	}
}

fn merge_usage(acc: &mut Option<Usage>, u: Usage) {
	fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
		match (a, b) {
			(None, None) => None,
			(a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
		}
	}
	let Some(a) = acc.as_mut() else {
		*acc = Some(u);
		return;
	};
	a.prompt_tokens += u.prompt_tokens;
	a.completion_tokens += u.completion_tokens;
	a.total_tokens += u.total_tokens;
	a.cached_tokens = add(a.cached_tokens, u.cached_tokens);
	a.cache_write_tokens = add(a.cache_write_tokens, u.cache_write_tokens);
	a.reasoning_tokens = add(a.reasoning_tokens, u.reasoning_tokens);
	a.cost_usd = match (a.cost_usd, u.cost_usd) {
		(None, None) => None,
		(x, y) => Some(x.unwrap_or(0.0) + y.unwrap_or(0.0)),
	};
	a.latency_ms = add(a.latency_ms, u.latency_ms);
	// 合算に TTFT は意味を持たない。request_id は最後のものを残す。
	a.ttft_ms = None;
	a.request_id = u.request_id.or(a.request_id.take());
}

#[cfg(test)]
mod tests {
	use std::sync::{Arc, Mutex};

	use super::*;
	use crate::{Completion, ModelInfo, Tool, ToolSpec};

	/// 台本どおりに Completion を返し、受け取ったリクエストを記録するプロバイダ。
	struct Scripted {
		turns: Mutex<std::collections::VecDeque<Completion>>,
		seen: Mutex<Vec<Vec<ChatMessage>>>,
	}

	impl Scripted {
		fn new(turns: Vec<Completion>) -> Self {
			Self {
				turns: Mutex::new(turns.into()),
				seen: Mutex::default(),
			}
		}
	}

	fn call(id: &str, name: &str) -> ToolCall {
		ToolCall::new(id, name, serde_json::json!({"x": 1}))
	}

	fn turn(text: &str, calls: Vec<ToolCall>) -> Completion {
		Completion {
			text: text.into(),
			tool_calls: calls,
			usage: Some(Usage {
				prompt_tokens: 10,
				completion_tokens: 2,
				total_tokens: 12,
				..Default::default()
			}),
			..Default::default()
		}
	}

	#[async_trait::async_trait]
	impl Provider for Scripted {
		fn name(&self) -> &'static str {
			"scripted"
		}
		async fn list_models(&self) -> Result<Vec<ModelInfo>> {
			Ok(vec![])
		}
		async fn chat(&self, _: &str, _: Vec<ChatMessage>, _: Option<u32>) -> Result<String> {
			unimplemented!()
		}
		async fn chat_stream(
			&self,
			_: &str,
			_: Vec<ChatMessage>,
			_: &mut (dyn for<'a> FnMut(&'a str) + Send),
			_: &mut (dyn for<'a> FnMut(&'a str) + Send),
		) -> Result<String> {
			unimplemented!()
		}
		async fn responses(&self, _: &str, _: &str) -> Result<String> {
			unimplemented!()
		}
		async fn messages(&self, _: &str, _: Vec<ChatMessage>, _: Option<&str>, _: u32) -> Result<String> {
			unimplemented!()
		}
		async fn chat_with_tools(
			&self,
			_: &str,
			messages: Vec<ChatMessage>,
			_: &[ToolSpec],
			_: Option<u32>,
		) -> Result<Completion> {
			self.seen.lock().unwrap().push(messages);
			Ok(self.turns.lock().unwrap().pop_front().expect("台本が尽きた"))
		}
	}

	struct Echo {
		approval: bool,
		runs: Arc<Mutex<u32>>,
	}

	#[async_trait::async_trait]
	impl Tool for Echo {
		fn spec(&self) -> ToolSpec {
			ToolSpec {
				name: if self.approval { "danger" } else { "echo" }.into(),
				description: "".into(),
				input_schema: serde_json::json!({"type": "object"}),
				requires_approval: self.approval,
			}
		}
		async fn call(&self, args: serde_json::Value) -> ToolOutput {
			*self.runs.lock().unwrap() += 1;
			ToolOutput::ok(format!("ran {args}"))
		}
	}

	#[derive(Default)]
	struct Rec {
		allow: bool,
		events: Vec<String>,
	}

	#[async_trait::async_trait]
	impl RunHandler for Rec {
		fn on_text(&mut self, t: &str) {
			self.events.push(format!("text:{t}"));
		}
		fn on_tool_call(&mut self, c: &ToolCall) {
			self.events.push(format!("call:{}", c.name));
		}
		fn on_tool_result(&mut self, c: &ToolCall, o: &ToolOutput) {
			self.events.push(format!("result:{}:{}", c.name, o.is_error));
		}
		async fn approve(&mut self, _: &ToolCall, _: &ToolSpec) -> bool {
			self.allow
		}
	}

	fn registry(runs: &Arc<Mutex<u32>>) -> ToolRegistry {
		ToolRegistry::new()
			.with(Arc::new(Echo {
				approval: false,
				runs: runs.clone(),
			}))
			.with(Arc::new(Echo {
				approval: true,
				runs: runs.clone(),
			}))
	}

	fn req() -> LoopRequest {
		LoopRequest::new("m", vec![ChatMessage::user("go")])
	}

	#[tokio::test]
	async fn loops_until_final_answer_and_threads_results() {
		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![
			turn("調べる", vec![call("c1", "echo"), call("", "echo")]),
			turn("done", vec![]),
		]);
		let mut h = Rec::default();
		let out = run_loop(&p, &registry(&runs), &mut h, req()).await.unwrap();

		assert_eq!(out.text, "done");
		assert_eq!(out.steps, 2);
		assert!(!out.hit_step_limit);
		assert_eq!(*runs.lock().unwrap(), 2);
		// 使用量は2ステップ合算
		assert_eq!(out.usage.unwrap().prompt_tokens, 20);
		// user, assistant(tool_calls), tool, tool, assistant(final)
		let roles: Vec<_> = out.messages.iter().map(|m| m.role.as_str()).collect();
		assert_eq!(roles, ["user", "assistant", "tool", "tool", "assistant"]);
		assert_eq!(out.messages[2].tool_call_id.as_deref(), Some("c1"));
		// ID無しの呼び出しは補われ、assistant側と結果側で一致する
		let filled = &out.messages[1].tool_calls[1].id;
		assert_eq!(filled, "call_1_1");
		assert_eq!(out.messages[3].tool_call_id.as_deref(), Some(filled.as_str()));
		// 2回目のリクエストには結果が含まれている
		assert_eq!(p.seen.lock().unwrap()[1].len(), 4);
		assert_eq!(
			h.events,
			[
				"text:調べる",
				"call:echo",
				"result:echo:false",
				"call:echo",
				"result:echo:false"
			]
		);
	}

	#[tokio::test]
	async fn approval_is_denied_by_default_and_error_is_fed_back() {
		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![turn("", vec![call("c1", "danger")]), turn("ok", vec![])]);
		let mut h = Rec::default(); // allow = false
		let out = run_loop(&p, &registry(&runs), &mut h, req()).await.unwrap();

		assert_eq!(*runs.lock().unwrap(), 0, "拒否時は実行されない");
		assert!(out.messages[2].content.starts_with("Error: denied"));
		assert!(h.events.contains(&"result:danger:true".to_string()));

		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![turn("", vec![call("c1", "danger")]), turn("ok", vec![])]);
		let mut h = Rec {
			allow: true,
			..Default::default()
		};
		run_loop(&p, &registry(&runs), &mut h, req()).await.unwrap();
		assert_eq!(*runs.lock().unwrap(), 1);
	}

	#[tokio::test]
	async fn auto_approve_skips_handler() {
		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![turn("", vec![call("c1", "danger")]), turn("ok", vec![])]);
		let mut h = Rec::default(); // allow = false でも聞かれない
		let mut r = req();
		r.approval = ApprovalPolicy::AutoApprove;
		run_loop(&p, &registry(&runs), &mut h, r).await.unwrap();
		assert_eq!(*runs.lock().unwrap(), 1);
	}

	#[tokio::test]
	async fn unknown_tool_is_reported_to_model_not_fatal() {
		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![turn("", vec![call("c1", "nope")]), turn("ok", vec![])]);
		let out = run_loop(&p, &registry(&runs), &mut Rec::default(), req())
			.await
			.unwrap();
		assert_eq!(out.messages[2].content, "Error: unknown tool: nope");
		assert_eq!(out.text, "ok");
	}

	#[tokio::test]
	async fn step_limit_stops_without_dangling_tool_calls() {
		let runs = Arc::new(Mutex::new(0));
		let p = Scripted::new(vec![
			turn("", vec![call("c1", "echo")]),
			turn("まだ", vec![call("c2", "echo")]),
		]);
		let mut r = req();
		r.max_steps = 2;
		let out = run_loop(&p, &registry(&runs), &mut Rec::default(), r).await.unwrap();
		assert!(out.hit_step_limit);
		assert_eq!(out.steps, 2);
		assert_eq!(*runs.lock().unwrap(), 1, "上限ターンの要求は実行しない");
		// 末尾は実行済みの tool 結果で、未応答の tool_calls は残らない
		assert_eq!(out.messages.last().unwrap().role, "tool");

		let mut r = req();
		r.max_steps = 0;
		let err = run_loop(&p, &registry(&runs), &mut Rec::default(), r).await;
		assert!(matches!(err, Err(Error::Config(_))));
	}
}
