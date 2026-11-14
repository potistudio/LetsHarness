//! ツール呼び出しの wire 変換 (リクエスト組み立てとレスポンス解析)。
//!
//! HTTP に触れない純粋関数だけを置き、単体テストできるようにしてある。
//! - `claude-*`   : Anthropic Messages (`tool_use` / `tool_result`)
//! - それ以外      : OpenAI Chat Completions (`tool_calls` / `role: tool`)

use letsharness_kernel::{ChatMessage, Completion, ToolCall, ToolSpec};

use crate::{extract_chat_message_thinking, extract_messages_thinking};

/// モデルが返した引数文字列を JSON に。空は `{}`、壊れていれば生文字列を
/// `Value::String` のまま残す (ツール側が「引数が不正」とモデルへ返せる)。
fn parse_tool_arguments(raw: &str) -> serde_json::Value {
	if raw.trim().is_empty() {
		return serde_json::json!({});
	}
	serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// OpenAI Chat Completions 形のメッセージ配列。
fn chat_tools_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
	messages
		.iter()
		.map(|m| match m.role.as_str() {
			"assistant" if !m.tool_calls.is_empty() => serde_json::json!({
				"role": "assistant",
				"content": if m.content.is_empty() { serde_json::Value::Null } else { m.content.clone().into() },
				"tool_calls": m.tool_calls.iter().map(|c| serde_json::json!({
					"id": c.id,
					"type": "function",
					"function": {"name": c.name, "arguments": c.arguments.to_string()},
				})).collect::<Vec<_>>(),
			}),
			"tool" => serde_json::json!({
				"role": "tool",
				"tool_call_id": m.tool_call_id,
				"content": m.content,
			}),
			role => serde_json::json!({"role": role, "content": m.content}),
		})
		.collect()
}

pub(crate) fn build_chat_tools_body(
	model: &str,
	messages: &[ChatMessage],
	tools: &[ToolSpec],
	max_tokens: Option<u32>,
) -> serde_json::Value {
	let mut body = serde_json::json!({
		"model": model,
		"messages": chat_tools_messages(messages),
		"stream": false,
	});
	if !tools.is_empty() {
		body["tools"] = tools
			.iter()
			.map(|t| {
				serde_json::json!({
					"type": "function",
					"function": {"name": t.name, "description": t.description, "parameters": t.input_schema},
				})
			})
			.collect();
	}
	if let Some(n) = max_tokens {
		body["max_tokens"] = serde_json::json!(n);
	}
	body
}

pub(crate) fn parse_chat_tools_response(v: &serde_json::Value) -> Completion {
	let message = v.pointer("/choices/0/message");
	let text = message
		.and_then(|m| m.get("content"))
		.and_then(|c| c.as_str())
		.unwrap_or_default()
		.to_string();
	let tool_calls = message
		.and_then(|m| m.get("tool_calls"))
		.and_then(|t| t.as_array())
		.map(|calls| {
			calls
				.iter()
				.filter_map(|c| {
					let name = c.pointer("/function/name")?.as_str()?.to_string();
					let raw = c.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("");
					Some(ToolCall::new(
						c.get("id").and_then(|i| i.as_str()).unwrap_or_default(),
						name,
						parse_tool_arguments(raw),
					))
				})
				.collect()
		})
		.unwrap_or_default();
	Completion {
		text,
		thinking: message.and_then(extract_chat_message_thinking),
		tool_calls,
		usage: None,
	}
}

/// Anthropic Messages 形: `(system, messages)`。
///
/// - `system` ロールはトップレベル `system` に集約する。
/// - assistant のツール呼び出しは `tool_use` ブロック。
/// - `tool` 結果は user メッセージの `tool_result` ブロック。直前も結果なら
///   同じ user メッセージへまとめる (1つの assistant ターンへの結果は1メッセージで返す必要がある)。
fn messages_tools_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<serde_json::Value>) {
	let mut system: Vec<&str> = Vec::new();
	let mut out: Vec<serde_json::Value> = Vec::new();
	let mut last_is_tool_results = false;
	for m in messages {
		let is_result = m.role == "tool";
		match m.role.as_str() {
			"system" => system.push(&m.content),
			"tool" => {
				let block = serde_json::json!({
					"type": "tool_result",
					"tool_use_id": m.tool_call_id,
					"content": m.content,
					// 失敗はエージェントループが `Error:` 接頭辞で表す。
					"is_error": m.content.starts_with("Error: "),
				});
				match out.last_mut().filter(|_| last_is_tool_results) {
					Some(prev) => prev["content"].as_array_mut().expect("tool_result message").push(block),
					None => out.push(serde_json::json!({"role": "user", "content": [block]})),
				}
			}
			"assistant" if !m.tool_calls.is_empty() => {
				let mut blocks = Vec::new();
				if !m.content.is_empty() {
					blocks.push(serde_json::json!({"type": "text", "text": m.content}));
				}
				blocks.extend(m.tool_calls.iter().map(
					|c| serde_json::json!({"type": "tool_use", "id": c.id, "name": c.name, "input": c.arguments}),
				));
				out.push(serde_json::json!({"role": "assistant", "content": blocks}));
			}
			role => out.push(serde_json::json!({"role": role, "content": m.content})),
		}
		last_is_tool_results = is_result;
	}
	((!system.is_empty()).then(|| system.join("\n\n")), out)
}

pub(crate) fn build_messages_tools_body(
	model: &str,
	messages: &[ChatMessage],
	tools: &[ToolSpec],
	max_tokens: Option<u32>,
) -> serde_json::Value {
	let (system, msgs) = messages_tools_messages(messages);
	let mut body = serde_json::json!({
		"model": model,
		// Messages APIは max_tokens 必須。
		"max_tokens": max_tokens.unwrap_or(4096),
		"messages": msgs,
	});
	if let Some(s) = system {
		body["system"] = serde_json::json!(s);
	}
	if !tools.is_empty() {
		body["tools"] = tools
			.iter()
			.map(|t| serde_json::json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
			.collect();
	}
	body
}

pub(crate) fn parse_messages_tools_response(v: &serde_json::Value) -> Completion {
	let blocks = v
		.get("content")
		.and_then(|c| c.as_array())
		.map(Vec::as_slice)
		.unwrap_or(&[]);
	let mut text = String::new();
	let mut tool_calls = Vec::new();
	for b in blocks {
		match b.get("type").and_then(|t| t.as_str()) {
			Some("text") => text.push_str(b.get("text").and_then(|t| t.as_str()).unwrap_or_default()),
			Some("tool_use") => {
				let Some(name) = b.get("name").and_then(|n| n.as_str()) else {
					continue;
				};
				tool_calls.push(ToolCall::new(
					b.get("id").and_then(|i| i.as_str()).unwrap_or_default(),
					name,
					b.get("input").cloned().unwrap_or_else(|| serde_json::json!({})),
				));
			}
			_ => {}
		}
	}
	Completion {
		text,
		thinking: extract_messages_thinking(v),
		tool_calls,
		usage: None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample_tools() -> Vec<ToolSpec> {
		vec![ToolSpec {
			name: "read_file".into(),
			description: "read".into(),
			input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
		}]
	}

	fn sample_history() -> Vec<ChatMessage> {
		let calls = vec![
			ToolCall::new("c1", "read_file", serde_json::json!({"path": "a"})),
			ToolCall::new("c2", "read_file", serde_json::json!({"path": "b"})),
		];
		vec![
			ChatMessage::system("sys"),
			ChatMessage::user("hi"),
			ChatMessage::assistant_tool_calls("", calls),
			ChatMessage::tool_result("c1", "A"),
			ChatMessage::tool_result("c2", "Error: nope"),
		]
	}

	#[test]
	fn chat_tools_request_shape() {
		let body = build_chat_tools_body("m", &sample_history(), &sample_tools(), Some(10));
		assert_eq!(body["tools"][0]["function"]["name"], "read_file");
		assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
		assert_eq!(body["max_tokens"], 10);
		let msgs = body["messages"].as_array().unwrap();
		assert_eq!(msgs[0], serde_json::json!({"role": "system", "content": "sys"}));
		assert!(msgs[2]["content"].is_null());
		// arguments は JSON 文字列で渡す
		assert_eq!(msgs[2]["tool_calls"][1]["function"]["arguments"], "{\"path\":\"b\"}");
		assert_eq!(msgs[3]["role"], "tool");
		assert_eq!(msgs[3]["tool_call_id"], "c1");
		// ツール無しなら tools キーを付けない
		assert!(build_chat_tools_body("m", &[], &[], None).get("tools").is_none());
	}

	#[test]
	fn chat_tools_response_parses_calls_and_bad_arguments() {
		let v = serde_json::json!({"choices": [{"message": {
			"content": null,
			"tool_calls": [
				{"id": "c1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}},
				{"id": "c2", "type": "function", "function": {"name": "read_file", "arguments": "{oops"}},
				{"id": "c3", "type": "function", "function": {"name": "read_file", "arguments": ""}},
			]
		}}]});
		let c = parse_chat_tools_response(&v);
		assert_eq!(c.text, "");
		assert_eq!(c.tool_calls.len(), 3);
		assert_eq!(c.tool_calls[0].arguments, serde_json::json!({"path": "a"}));
		assert_eq!(c.tool_calls[1].arguments, serde_json::json!("{oops"));
		assert_eq!(c.tool_calls[2].arguments, serde_json::json!({}));

		let plain = parse_chat_tools_response(&serde_json::json!({"choices": [{"message": {"content": "hi"}}]}));
		assert_eq!(plain.text, "hi");
		assert!(plain.tool_calls.is_empty());
	}

	#[test]
	fn messages_tools_request_merges_results_and_lifts_system() {
		let body = build_messages_tools_body("claude-x", &sample_history(), &sample_tools(), None);
		assert_eq!(body["system"], "sys");
		assert_eq!(body["max_tokens"], 4096);
		assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
		let msgs = body["messages"].as_array().unwrap();
		// user, assistant(tool_use x2), user(tool_result x2)
		assert_eq!(msgs.len(), 3);
		assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
		assert_eq!(msgs[1]["content"][1]["input"], serde_json::json!({"path": "b"}));
		let results = msgs[2]["content"].as_array().unwrap();
		assert_eq!(msgs[2]["role"], "user");
		assert_eq!(results.len(), 2);
		assert_eq!(results[0]["tool_use_id"], "c1");
		assert_eq!(results[0]["is_error"], false);
		assert_eq!(results[1]["is_error"], true);
	}

	#[test]
	fn messages_tools_response_parses_tool_use() {
		let v = serde_json::json!({"content": [
			{"type": "text", "text": "読む"},
			{"type": "tool_use", "id": "tu1", "name": "read_file", "input": {"path": "a"}},
		]});
		let c = parse_messages_tools_response(&v);
		assert_eq!(c.text, "読む");
		assert_eq!(c.tool_calls[0].id, "tu1");
		assert_eq!(c.tool_calls[0].arguments, serde_json::json!({"path": "a"}));
	}
}
