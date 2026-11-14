//! `agy -p --output-format stream-json` の入出力変換。
//!
//! プロセスに触れない純粋な部分だけを置き、単体テストできるようにしてある。
//!
//! 出力は1行1JSON (NDJSON):
//! - `init`        : 開始時に1回 (`cwd` / `tools` / `model` など)
//! - `step_update` : ステップ遷移・テキスト差分
//!   (`step_index` / `state` / `step_type` / `text_delta` / `tool_name` / `tool_info` …)
//! - `result`      : 終了時に1回。`--output-format json` の封筒と同じ形
//!   (`status` / `response` / `error` / `usage` / `conversation_id` …)
//!
//! ペイロードは `{"event": "<name>", "<name>": {...}}` の形を基本としつつ、
//! フィールドがトップレベルに平置きされていても読めるようにしてある。

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use letsharness_kernel::{AgentHandler, ChatMessage, ToolCall, ToolOutput, Usage};

// -----------------------------------------------------------------------------
// 入力: プロンプト
// -----------------------------------------------------------------------------

/// kernel のメッセージ列を agy に渡す1つのプロンプトにする。
///
/// agy には system プロンプトや会話履歴を渡す口が無いため、テキストに畳む。
/// system も履歴も無ければ最後の user 本文をそのまま渡す。
pub(crate) fn render_prompt(messages: &[ChatMessage]) -> String {
	let system: Vec<&str> = messages
		.iter()
		.filter(|m| m.role == "system")
		.map(|m| m.content.as_str())
		.collect();
	let mut turns: Vec<&ChatMessage> = messages.iter().filter(|m| m.role != "system").collect();
	// 最後の user を「今回の依頼」として切り出す。
	let last = match turns.last() {
		Some(m) if m.role == "user" => turns.pop().map(|m| m.content.as_str()),
		_ => None,
	};
	if system.is_empty() && turns.is_empty() {
		return last.unwrap_or_default().to_string();
	}

	let mut out = String::new();
	if !system.is_empty() {
		out.push_str("<system>\n");
		out.push_str(&system.join("\n\n"));
		out.push_str("\n</system>\n\n");
	}
	if !turns.is_empty() {
		out.push_str("<conversation>\n");
		for m in turns {
			let role = if m.role == "tool" {
				"tool result"
			} else {
				m.role.as_str()
			};
			out.push_str(&format!("[{role}]\n{}\n\n", m.content));
		}
		out.push_str("</conversation>\n\n");
	}
	if let Some(last) = last {
		out.push_str(last);
	}
	out.trim_end().to_string()
}

/// 会話を再開するときのプロンプト。履歴は agy 側にあるので最後の user 本文だけ。
pub(crate) fn resume_prompt(messages: &[ChatMessage]) -> Option<&str> {
	messages
		.iter()
		.rev()
		.find(|m| m.role == "user")
		.map(|m| m.content.as_str())
}

// -----------------------------------------------------------------------------
// 出力: イベント
// -----------------------------------------------------------------------------

/// 1行分の解釈結果。
#[derive(Debug, PartialEq)]
pub(crate) enum Event {
	Step(Step),
	Result(RunResult),
	/// `init` や未知のイベント。
	Other,
}

/// `step_update` 1件。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Step {
	pub index: u64,
	/// `ACTIVE` / `DONE`。
	pub state: String,
	/// `user_input` / `agent_response` / `tool` / `checkpoint` / (思考系) …
	pub step_type: String,
	pub text_delta: Option<String>,
	pub tool: Option<ToolInfo>,
}

impl Step {
	fn is_done(&self) -> bool {
		self.state.eq_ignore_ascii_case("DONE")
	}
}

/// ツールステップの中身 (`tool_info` + `tool_name`)。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ToolInfo {
	pub name: String,
	pub parameters: serde_json::Value,
	pub output: Option<String>,
	pub error: Option<String>,
}

#[derive(Debug, PartialEq)]
enum StepKind {
	Text,
	Thinking,
	Tool,
	/// ユーザー入力のエコー・チェックポイント・サブエージェントなど。
	Ignored,
}

fn step_kind(step_type: &str) -> StepKind {
	match step_type {
		"agent_response" => StepKind::Text,
		"tool" => StepKind::Tool,
		t if t.contains("thought") || t.contains("thinking") || t.contains("reasoning") => StepKind::Thinking,
		_ => StepKind::Ignored,
	}
}

/// `result` イベント (= `--output-format json` の封筒)。
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RunResult {
	pub status: String,
	pub response: String,
	pub error: Option<String>,
	pub conversation_id: Option<String>,
	pub usage: Option<UsageCounts>,
}

impl RunResult {
	pub fn is_success(&self) -> bool {
		self.status.eq_ignore_ascii_case("SUCCESS")
	}
}

/// agy の `usage` オブジェクトそのまま。正規化は [`UsageCounts::to_usage`]。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct UsageCounts {
	pub input: u64,
	pub output: u64,
	pub thinking: Option<u64>,
	pub cache_read: Option<u64>,
	pub total: Option<u64>,
}

impl UsageCounts {
	fn from_value(v: &serde_json::Value) -> Option<Self> {
		let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
		Some(Self {
			input: n("input_tokens")?,
			output: n("output_tokens").unwrap_or(0),
			thinking: n("thinking_tokens"),
			cache_read: n("cache_read_tokens"),
			total: n("total_tokens"),
		})
	}

	/// kernel の [`Usage`] に。
	///
	/// - 入力 = `input_tokens` (Gemini と同じくキャッシュ読み込みを含む前提)
	/// - 出力 = `output_tokens` + `thinking_tokens` (思考は別計上で報告される)
	///
	/// agy は内部で複数回モデルを呼ぶため、値はその合算。
	pub fn to_usage(self) -> Usage {
		let completion = self.output + self.thinking.unwrap_or(0);
		Usage {
			prompt_tokens: self.input,
			completion_tokens: completion,
			total_tokens: self.total.unwrap_or(self.input + completion),
			cached_tokens: self.cache_read,
			reasoning_tokens: self.thinking,
			..Default::default()
		}
	}
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
	v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

/// 文字列ならそのまま、それ以外 (オブジェクトなど) は JSON 文字列に。null は `None`。
fn text_of(v: &serde_json::Value) -> Option<String> {
	match v {
		serde_json::Value::Null => None,
		serde_json::Value::String(s) => Some(s.clone()),
		other => Some(other.to_string()),
	}
}

fn parse_tool(payload: &serde_json::Value) -> Option<ToolInfo> {
	let info = payload.get("tool_info");
	let name = info
		.and_then(|i| str_field(i, "name"))
		.or_else(|| str_field(payload, "tool_name"))?;
	let error = info.and_then(|i| i.get("error")).and_then(|e| {
		// `{type, message}` が基本。文字列でも受ける。
		let msg = e.get("message").and_then(text_of).or_else(|| text_of(e))?;
		Some(match e.get("type").and_then(|t| t.as_str()) {
			Some(t) => format!("{t}: {msg}"),
			None => msg,
		})
	});
	Some(ToolInfo {
		name,
		parameters: info
			.and_then(|i| i.get("parameters"))
			.cloned()
			.unwrap_or_else(|| serde_json::json!({})),
		output: info.and_then(|i| i.get("output")).and_then(text_of),
		error,
	})
}

/// NDJSON の1行を解釈する。JSON でない行は `None` (呼び出し側で無視する)。
pub(crate) fn parse_line(line: &str) -> Option<Event> {
	let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
	let name = v.get("event").or_else(|| v.get("type")).and_then(|e| e.as_str())?;
	// `{"event": "x", "x": {...}}` を基本に、平置きも許容する。
	let payload = v.get(name).filter(|p| p.is_object()).unwrap_or(&v);
	Some(match name {
		"step_update" => Event::Step(Step {
			index: payload.get("step_index").and_then(|i| i.as_u64()).unwrap_or(0),
			state: str_field(payload, "state").unwrap_or_default(),
			step_type: str_field(payload, "step_type").unwrap_or_default(),
			text_delta: str_field(payload, "text_delta").filter(|d| !d.is_empty()),
			tool: parse_tool(payload),
		}),
		"result" => Event::Result(RunResult {
			status: str_field(payload, "status").unwrap_or_default(),
			response: str_field(payload, "response").unwrap_or_default(),
			error: str_field(payload, "error").filter(|e| !e.is_empty()),
			conversation_id: str_field(payload, "conversation_id").or_else(|| str_field(&v, "conversation_id")),
			usage: payload.get("usage").and_then(UsageCounts::from_value),
		}),
		_ => Event::Other,
	})
}

// -----------------------------------------------------------------------------
// ステップ列 -> AgentHandler
// -----------------------------------------------------------------------------

/// `step_update` の列を [`AgentHandler`] の通知に変換する。
///
/// 組み込みループと同じ意味になるよう揃える:
/// - 思考ステップは完了時にまとめて `on_thinking`。
/// - `agent_response` は、後にツールが続けば「前置き」として `on_text`。
///   最後のものは最終回答なので通知せず [`StepTracker::finish`] で返す。
/// - ツールは開始時に `on_tool_call`、完了時に `on_tool_result` (実行するのは agy)。
#[derive(Default)]
pub(crate) struct StepTracker {
	/// 進行中のテキスト系ステップ (index, 思考か, 本文)。
	open: Option<(u64, bool, String)>,
	/// 完了済みで、まだ最終回答か前置きか決まっていない本文。
	pending_text: String,
	/// 通知済みで結果待ちのツール呼び出し (step_index 順)。
	tools: BTreeMap<u64, ToolCall>,
	/// 観測したツールステップ数。
	tool_steps: usize,
}

impl StepTracker {
	pub fn on_step(&mut self, step: Step, h: &mut dyn AgentHandler) {
		if self.open.as_ref().is_some_and(|(i, ..)| *i != step.index) {
			self.close_open(h);
		}
		match step_kind(&step.step_type) {
			kind @ (StepKind::Text | StepKind::Thinking) => {
				let thinking = kind == StepKind::Thinking;
				let (_, _, buf) = self.open.get_or_insert_with(|| (step.index, thinking, String::new()));
				if let Some(d) = &step.text_delta {
					buf.push_str(d);
				}
				if step.is_done() {
					self.close_open(h);
				}
			}
			StepKind::Tool => {
				// ツールが続く = 直前の本文は前置き。
				if !self.pending_text.trim().is_empty() {
					h.on_text(self.pending_text.trim());
				}
				self.pending_text.clear();
				let done = step.is_done();
				let Some(info) = step.tool else {
					return;
				};
				if let Entry::Vacant(slot) = self.tools.entry(step.index) {
					let call = ToolCall::new(format!("agy_step_{}", step.index), info.name, info.parameters);
					h.on_tool_call(&call);
					slot.insert(call);
					self.tool_steps += 1;
				}
				if done && let Some(call) = self.tools.remove(&step.index) {
					let output = match info.error {
						Some(e) => ToolOutput::error(e),
						None => ToolOutput::ok(info.output.unwrap_or_default()),
					};
					h.on_tool_result(&call, &output);
				}
			}
			StepKind::Ignored => {}
		}
	}

	fn close_open(&mut self, h: &mut dyn AgentHandler) {
		let Some((_, thinking, text)) = self.open.take() else {
			return;
		};
		if thinking {
			if !text.trim().is_empty() {
				h.on_thinking(&text);
			}
		} else if !text.is_empty() {
			if !self.pending_text.is_empty() {
				self.pending_text.push_str("\n\n");
			}
			self.pending_text.push_str(&text);
		}
	}

	/// 残りを流し切り、ストリームから組み立てた最終回答とツールステップ数を返す。
	pub fn finish(mut self, h: &mut dyn AgentHandler) -> (String, usize) {
		self.close_open(h);
		// 完了が届かなかったツールも結果待ちのまま放置しない。
		for call in std::mem::take(&mut self.tools).into_values() {
			h.on_tool_result(&call, &ToolOutput::error("(agy から完了が報告されませんでした)"));
		}
		(self.pending_text, self.tool_steps)
	}
}

// -----------------------------------------------------------------------------
// モデル一覧
// -----------------------------------------------------------------------------

/// `agy models` の出力からモデルIDを抜く。1行1モデル (先頭トークン) を想定し、
/// 見出し (`...:`)・空行・ANSI エスケープは捨てる。
pub(crate) fn parse_models(stdout: &str) -> Vec<String> {
	strip_ansi(stdout)
		.lines()
		.map(|l| l.trim().trim_start_matches(['-', '*', '•']))
		.filter(|l| !l.ends_with(':'))
		.filter_map(|l| l.split_whitespace().next())
		.map(str::to_string)
		.collect()
}

fn strip_ansi(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut chars = s.chars().peekable();
	while let Some(c) = chars.next() {
		if c == '\u{1b}' && chars.peek() == Some(&'[') {
			chars.next();
			// CSI: パラメータが続き、0x40..=0x7E の1文字で終わる。
			for c in chars.by_ref() {
				if ('\u{40}'..='\u{7e}').contains(&c) {
					break;
				}
			}
			continue;
		}
		out.push(c);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn prompt_plain_user_is_passed_through() {
		assert_eq!(render_prompt(&[ChatMessage::user("hi")]), "hi");
	}

	#[test]
	fn prompt_folds_system_and_history() {
		let p = render_prompt(&[
			ChatMessage::system("be brief"),
			ChatMessage::user("q1"),
			ChatMessage::assistant("a1"),
			ChatMessage::user("q2"),
		]);
		assert_eq!(
			p,
			"<system>\nbe brief\n</system>\n\n<conversation>\n[user]\nq1\n\n[assistant]\na1\n\n</conversation>\n\nq2"
		);
		assert_eq!(
			render_prompt(&[ChatMessage::system("s"), ChatMessage::user("q")]),
			"<system>\ns\n</system>\n\nq"
		);
		let msgs = [
			ChatMessage::user("q1"),
			ChatMessage::assistant("a1"),
			ChatMessage::user("q2"),
		];
		assert_eq!(resume_prompt(&msgs), Some("q2"));
		assert_eq!(resume_prompt(&[ChatMessage::system("s")]), None);
	}

	#[test]
	fn parses_steps() {
		let text = r#"{"event":"step_update","step_update":{"conversation_id":"c","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hel"}}"#;
		let Some(Event::Step(s)) = parse_line(text) else {
			panic!("not a step");
		};
		assert_eq!(
			(s.index, s.state.as_str(), s.step_type.as_str()),
			(1, "ACTIVE", "agent_response")
		);
		assert_eq!(s.text_delta.as_deref(), Some("Hel"));
		assert!(s.tool.is_none());
		// 平置きでも読める
		let flat = r#"{"event":"step_update","step_index":2,"step_type":"agent_response","text_delta":"lo"}"#;
		assert!(matches!(parse_line(flat), Some(Event::Step(Step { index: 2, .. }))));

		let tool = r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"path":"a"},"error":{"type":"NOT_FOUND","message":"no such file"}}}}"#;
		let Some(Event::Step(s)) = parse_line(tool) else {
			panic!("not a step");
		};
		let t = s.tool.unwrap();
		assert_eq!(t.name, "read_file");
		assert_eq!(t.parameters, serde_json::json!({"path": "a"}));
		assert_eq!(t.error.as_deref(), Some("NOT_FOUND: no such file"));
		// tool_info が無くても tool_name だけで拾う / 出力がオブジェクトなら JSON 文字列に
		let tool = r#"{"event":"step_update","step_update":{"step_type":"tool","tool_name":"ls","tool_info":{"output":{"files":1}}}}"#;
		let Some(Event::Step(s)) = parse_line(tool) else {
			panic!("not a step");
		};
		let t = s.tool.unwrap();
		assert_eq!((t.name.as_str(), t.output.as_deref()), ("ls", Some(r#"{"files":1}"#)));

		assert_eq!(parse_line(r#"{"event":"init","init":{"cwd":"/"}}"#), Some(Event::Other));
		assert_eq!(parse_line("not json"), None);
		assert_eq!(parse_line(r#"{"no_event":1}"#), None);
	}

	#[test]
	fn parses_result_and_usage() {
		let line = r#"{"event":"result","conversation_id":"conv1","result":{"status":"SUCCESS","response":"Hello","duration_seconds":1.5,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":20,"thinking_tokens":30,"cache_read_tokens":40,"total_tokens":150}}}"#;
		let Some(Event::Result(r)) = parse_line(line) else {
			panic!("not a result");
		};
		assert!(r.is_success());
		assert_eq!(r.response, "Hello");
		assert_eq!(r.conversation_id.as_deref(), Some("conv1"));
		let u = r.usage.unwrap().to_usage();
		assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens), (100, 50, 150));
		assert_eq!((u.cached_tokens, u.reasoning_tokens), (Some(40), Some(30)));

		let err = r#"{"event":"result","result":{"status":"ERROR","error":"model not found"}}"#;
		let Some(Event::Result(r)) = parse_line(err) else {
			panic!("not a result");
		};
		assert!(!r.is_success());
		assert_eq!(r.error.as_deref(), Some("model not found"));
		assert!(r.usage.is_none());
	}

	#[derive(Default)]
	struct Rec(Vec<String>);

	#[async_trait::async_trait]
	impl AgentHandler for Rec {
		fn on_text(&mut self, t: &str) {
			self.0.push(format!("text:{t}"));
		}
		fn on_thinking(&mut self, t: &str) {
			self.0.push(format!("thinking:{t}"));
		}
		fn on_tool_call(&mut self, c: &ToolCall) {
			self.0.push(format!("call:{}:{}:{}", c.id, c.name, c.arguments));
		}
		fn on_tool_result(&mut self, c: &ToolCall, o: &ToolOutput) {
			self.0.push(format!("result:{}:{}:{}", c.id, o.is_error, o.content));
		}
	}

	fn step(index: u64, state: &str, step_type: &str, delta: Option<&str>, tool: Option<ToolInfo>) -> Step {
		Step {
			index,
			state: state.into(),
			step_type: step_type.into(),
			text_delta: delta.map(str::to_string),
			tool,
		}
	}

	fn tool(output: Option<&str>) -> Option<ToolInfo> {
		Some(ToolInfo {
			name: "read_file".into(),
			parameters: serde_json::json!({"path": "a"}),
			output: output.map(str::to_string),
			error: None,
		})
	}

	#[test]
	fn tracker_maps_steps_like_loop_agent() {
		let mut h = Rec::default();
		let mut t = StepTracker::default();
		for s in [
			step(0, "DONE", "user_input", Some("q"), None),
			step(1, "ACTIVE", "thinking", Some("考"), None),
			step(1, "ACTIVE", "thinking", Some("え中"), None),
			// 完了 (DONE) が来なくても次のステップで閉じる
			step(2, "ACTIVE", "agent_response", Some("読むで"), None),
			step(3, "ACTIVE", "tool", None, tool(None)),
			step(3, "DONE", "tool", None, tool(Some("A"))),
			step(4, "ACTIVE", "agent_response", Some("答え"), None),
			step(4, "DONE", "agent_response", Some("や"), None),
		] {
			t.on_step(s, &mut h);
		}
		let (text, tools) = t.finish(&mut h);
		assert_eq!(text, "答えや");
		assert_eq!(tools, 1);
		assert_eq!(
			h.0,
			[
				"thinking:考え中",
				"text:読むで",
				r#"call:agy_step_3:read_file:{"path":"a"}"#,
				"result:agy_step_3:false:A",
			]
		);
	}

	#[test]
	fn tracker_closes_unfinished_tools() {
		let mut h = Rec::default();
		let mut t = StepTracker::default();
		// ACTIVE を飛ばして DONE だけ届いても呼び出し→結果の順で通知する
		t.on_step(step(0, "DONE", "tool", None, tool(Some("x"))), &mut h);
		t.on_step(step(1, "ACTIVE", "tool", None, tool(None)), &mut h);
		let (text, tools) = t.finish(&mut h);
		assert_eq!((text.as_str(), tools), ("", 2));
		assert_eq!(
			h.0[..2],
			[
				"call:agy_step_0:read_file:{\"path\":\"a\"}",
				"result:agy_step_0:false:x"
			]
		);
		assert!(h.0[3].starts_with("result:agy_step_1:true:"), "{:?}", h.0);
	}

	#[test]
	fn models_listing() {
		let out = "\u{1b}[1mAvailable models:\u{1b}[0m\ngemini-3.8-flash-high\n  gemini-3.8-flash-low  (default)\n- claude-x\n\n";
		assert_eq!(
			parse_models(out),
			["gemini-3.8-flash-high", "gemini-3.8-flash-low", "claude-x"]
		);
	}
}
