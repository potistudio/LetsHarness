//! Gemini `generateContent` の wire 変換 (リクエスト組み立てとレスポンス解析)。
//!
//! HTTP に触れない純粋関数だけを置き、単体テストできるようにしてある。
//!
//! kernel の [`ChatMessage`] との対応:
//! - `system`    -> トップレベル `systemInstruction`
//! - `user`      -> `{role: user, parts: [{text}]}`
//! - `assistant` -> `{role: model, parts: [{text}, {functionCall, thoughtSignature}...]}`
//! - `tool`      -> `{role: user, parts: [{functionResponse}]}` (連続する結果は1つにまとめる)

use letsharness_kernel::{ChatMessage, Completion, ToolCall, ToolSpec, Usage};

/// Gemini 3 は functionCall パートに `thoughtSignature` が無いと 400 を返す。
/// 他プロバイダ由来の履歴など署名を持たない呼び出しに付ける、公式の検証スキップ値。
pub(crate) const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// [`ToolCall::extra`] 内のキー。
const EXTRA_SIGNATURE: &str = "thoughtSignature";
/// API が `functionCall.id` を返した場合だけ入れる (返していない ID を送り返さないため)。
const EXTRA_ID: &str = "id";

/// 思考過程の要約 (`includeThoughts`) を要求してよいモデルか。
///
/// 思考非対応モデルに `thinkingConfig` を送るとエラーになるため、
/// 既知の非対応系 (Gemini 1.x / 2.0 / Gemma) だけ外す。未知のモデルは対応とみなす。
pub(crate) fn supports_thinking(model: &str) -> bool {
	let id = model.rsplit('/').next().unwrap_or(model);
	!(id.starts_with("gemini-1.") || id.starts_with("gemini-2.0") || id.starts_with("gemma"))
}

/// `gemini-2.5-flash` / `models/gemini-2.5-flash` / `tunedModels/x` -> URL のリソース名。
pub(crate) fn model_resource(model: &str) -> String {
	if model.contains('/') {
		model.to_string()
	} else {
		format!("models/{model}")
	}
}

/// `models/gemini-2.5-flash` -> `gemini-2.5-flash`。
pub(crate) fn model_id(name: &str) -> &str {
	name.strip_prefix("models/").unwrap_or(name)
}

fn text_part(text: &str) -> serde_json::Value {
	serde_json::json!({"text": text})
}

/// assistant のツール呼び出しを `functionCall` パート列に。
///
/// 並列呼び出しでは最初の1つだけが署名を持つ。どれも署名を持たない
/// (= 他プロバイダ由来) なら、先頭に検証スキップ値を付ける。
fn function_call_parts(calls: &[ToolCall]) -> Vec<serde_json::Value> {
	let any_signed = calls.iter().any(|c| c.extra.get(EXTRA_SIGNATURE).is_some());
	calls
		.iter()
		.enumerate()
		.map(|(i, c)| {
			let args = if c.arguments.is_object() {
				c.arguments.clone()
			} else {
				serde_json::json!({})
			};
			let mut call = serde_json::json!({"name": c.name, "args": args});
			if let Some(id) = c.extra.get(EXTRA_ID) {
				call["id"] = id.clone();
			}
			let mut part = serde_json::json!({"functionCall": call});
			match c.extra.get(EXTRA_SIGNATURE) {
				Some(sig) => part[EXTRA_SIGNATURE] = sig.clone(),
				None if !any_signed && i == 0 => part[EXTRA_SIGNATURE] = SKIP_SIGNATURE.into(),
				None => {}
			}
			part
		})
		.collect()
}

/// `(systemInstruction, contents)`。
pub(crate) fn build_contents(messages: &[ChatMessage]) -> (Option<serde_json::Value>, Vec<serde_json::Value>) {
	let mut system: Vec<&str> = Vec::new();
	let mut contents: Vec<serde_json::Value> = Vec::new();
	let mut last_is_tool_results = false;
	for (idx, m) in messages.iter().enumerate() {
		let is_result = m.role == "tool";
		match m.role.as_str() {
			"system" => system.push(&m.content),
			"tool" => {
				// functionResponse には関数名が必須。ID から直前までの呼び出しを引く。
				let call = m.tool_call_id.as_deref().and_then(|id| {
					messages[..idx]
						.iter()
						.rev()
						.flat_map(|p| p.tool_calls.iter())
						.find(|c| c.id == id)
				});
				let name = call
					.map(|c| c.name.as_str())
					.or(m.tool_call_id.as_deref())
					.unwrap_or("unknown");
				// 失敗はエージェントループが `Error:` 接頭辞で表す。
				let response = match m.content.strip_prefix("Error: ") {
					Some(err) => serde_json::json!({"error": err}),
					None => serde_json::json!({"result": m.content}),
				};
				let mut fr = serde_json::json!({"name": name, "response": response});
				if let Some(id) = call.and_then(|c| c.extra.get(EXTRA_ID)) {
					fr["id"] = id.clone();
				}
				let part = serde_json::json!({"functionResponse": fr});
				match contents.last_mut().filter(|_| last_is_tool_results) {
					Some(prev) => prev["parts"]
						.as_array_mut()
						.expect("functionResponse content")
						.push(part),
					None => contents.push(serde_json::json!({"role": "user", "parts": [part]})),
				}
			}
			"assistant" => {
				let mut parts = Vec::new();
				if !m.content.is_empty() {
					parts.push(text_part(&m.content));
				}
				parts.extend(function_call_parts(&m.tool_calls));
				// 空パートは API が拒否するため、本文もツールも無いターンは落とす。
				if !parts.is_empty() {
					contents.push(serde_json::json!({"role": "model", "parts": parts}));
				}
			}
			_ => contents.push(serde_json::json!({"role": "user", "parts": [text_part(&m.content)]})),
		}
		last_is_tool_results = is_result;
	}
	let system = (!system.is_empty()).then(|| serde_json::json!({"parts": [text_part(&system.join("\n\n"))]}));
	(system, contents)
}

/// `generateContent` / `streamGenerateContent` 共通のリクエスト本文。
pub(crate) fn build_body(
	model: &str,
	messages: &[ChatMessage],
	tools: &[ToolSpec],
	max_tokens: Option<u32>,
	include_thoughts: bool,
) -> serde_json::Value {
	let (system, contents) = build_contents(messages);
	let mut body = serde_json::json!({"contents": contents});
	if let Some(s) = system {
		body["systemInstruction"] = s;
	}
	let mut config = serde_json::Map::new();
	if let Some(n) = max_tokens {
		config.insert("maxOutputTokens".into(), n.into());
	}
	if include_thoughts && supports_thinking(model) {
		config.insert("thinkingConfig".into(), serde_json::json!({"includeThoughts": true}));
	}
	if !config.is_empty() {
		body["generationConfig"] = config.into();
	}
	if !tools.is_empty() {
		// `parameters` (OpenAPI サブセット) ではなく JSON Schema をそのまま渡せる口を使う。
		let decls: Vec<_> = tools
			.iter()
			.map(|t| {
				serde_json::json!({
					"name": t.name,
					"description": t.description,
					"parametersJsonSchema": t.input_schema,
				})
			})
			.collect();
		body["tools"] = serde_json::json!([{"functionDeclarations": decls}]);
	}
	body
}

/// レスポンス (またはストリームの1チャンク) の中身。
#[derive(Debug, Default)]
pub(crate) struct Parsed {
	pub text: String,
	pub thinking: String,
	pub tool_calls: Vec<ToolCall>,
}

/// `candidates[0].content.parts` を本文・思考・ツール呼び出しに分ける。
pub(crate) fn parse_parts(v: &serde_json::Value) -> Parsed {
	let mut out = Parsed::default();
	let parts = v
		.pointer("/candidates/0/content/parts")
		.and_then(|p| p.as_array())
		.map(Vec::as_slice)
		.unwrap_or(&[]);
	for p in parts {
		if let Some(fc) = p.get("functionCall") {
			let Some(name) = fc.get("name").and_then(|n| n.as_str()) else {
				continue;
			};
			let mut extra = serde_json::Map::new();
			if let Some(sig) = p.get(EXTRA_SIGNATURE).filter(|s| s.is_string()) {
				extra.insert(EXTRA_SIGNATURE.into(), sig.clone());
			}
			let id = fc.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty());
			if let Some(id) = id {
				extra.insert(EXTRA_ID.into(), id.into());
			}
			let mut call = ToolCall::new(
				// ID が無ければ空のまま。エージェントループが補う。
				id.unwrap_or_default(),
				name,
				fc.get("args").cloned().unwrap_or_else(|| serde_json::json!({})),
			);
			if !extra.is_empty() {
				call.extra = extra.into();
			}
			out.tool_calls.push(call);
			continue;
		}
		let Some(text) = p.get("text").and_then(|t| t.as_str()) else {
			continue;
		};
		if p.get("thought").and_then(|t| t.as_bool()) == Some(true) {
			out.thinking.push_str(text);
		} else {
			out.text.push_str(text);
		}
	}
	out
}

/// 応答が空のときの理由 (`SAFETY` などでブロックされた場合)。正常終了なら `None`。
pub(crate) fn blocked_reason(v: &serde_json::Value) -> Option<String> {
	if let Some(r) = v.pointer("/promptFeedback/blockReason").and_then(|r| r.as_str()) {
		return Some(format!("prompt blocked: {r}"));
	}
	let reason = v.pointer("/candidates/0/finishReason").and_then(|r| r.as_str())?;
	match reason {
		"STOP" | "MAX_TOKENS" | "FINISH_REASON_UNSPECIFIED" => None,
		other => Some(format!("finishReason={other}")),
	}
}

/// `usageMetadata` を正規化する。
///
/// - 入力 = `promptTokenCount` (キャッシュ込み) + `toolUsePromptTokenCount`
/// - 出力 = `candidatesTokenCount` + `thoughtsTokenCount` (思考は候補に含まれない)
pub(crate) fn parse_usage(v: &serde_json::Value) -> Option<Usage> {
	let u = v.get("usageMetadata")?;
	let n = |k: &str| u.get(k).and_then(|x| x.as_u64());
	let prompt = n("promptTokenCount")? + n("toolUsePromptTokenCount").unwrap_or(0);
	let thoughts = n("thoughtsTokenCount");
	let completion = n("candidatesTokenCount").unwrap_or(0) + thoughts.unwrap_or(0);
	Some(Usage {
		prompt_tokens: prompt,
		completion_tokens: completion,
		total_tokens: n("totalTokenCount").unwrap_or(prompt + completion),
		cached_tokens: n("cachedContentTokenCount"),
		reasoning_tokens: thoughts,
		request_id: v.get("responseId").and_then(|r| r.as_str()).map(str::to_string),
		..Default::default()
	})
}

/// 非ストリーム応答を [`Completion`] に。
pub(crate) fn parse_completion(v: &serde_json::Value) -> Completion {
	let p = parse_parts(v);
	Completion {
		text: p.text,
		thinking: (!p.thinking.trim().is_empty()).then_some(p.thinking),
		tool_calls: p.tool_calls,
		usage: parse_usage(v),
	}
}

/// `{"error": {"code": 400, "message": "...", "status": "INVALID_ARGUMENT"}}`。
pub(crate) fn parse_error_body(body: &str) -> (Option<String>, String) {
	if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
		&& let Some(err) = v.get("error")
	{
		let code = err.get("status").and_then(|c| c.as_str()).map(str::to_string);
		let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or(body).to_string();
		return (code, msg);
	}
	(None, body.chars().take(500).collect())
}

pub(crate) fn hint_for_status(status: u16, message: String) -> String {
	let hint = match status {
		400 if message.contains("API key") => " (APIキーを確認: GEMINI_API_KEY)",
		400 if message.contains("thought_signature") || message.contains("thought signature") => {
			" (thoughtSignature が欠けている: 履歴のツール呼び出しを改変していないか確認)"
		}
		403 => " (APIキーの権限 / 利用可能リージョンを確認)",
		404 => " (モデルIDを確認: letsharness --provider gemini models)",
		429 => " (レート制限 / クォータ超過: backoffして再試行)",
		500 | 503 => " (一時的な上流障害: 再試行)",
		_ => "",
	};
	format!("{message}{hint}")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample_tools() -> Vec<ToolSpec> {
		vec![ToolSpec {
			name: "read_file".into(),
			description: "read".into(),
			input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
			requires_approval: false,
		}]
	}

	#[test]
	fn model_names() {
		assert_eq!(model_resource("gemini-2.5-flash"), "models/gemini-2.5-flash");
		assert_eq!(model_resource("models/gemini-2.5-flash"), "models/gemini-2.5-flash");
		assert_eq!(model_resource("tunedModels/abc"), "tunedModels/abc");
		assert_eq!(model_id("models/gemini-2.5-pro"), "gemini-2.5-pro");
		assert!(supports_thinking("gemini-2.5-flash"));
		assert!(supports_thinking("models/gemini-3-pro-preview"));
		assert!(!supports_thinking("gemini-2.0-flash"));
		assert!(!supports_thinking("models/gemma-3-27b-it"));
	}

	#[test]
	fn request_lifts_system_and_sets_config() {
		let msgs = vec![
			ChatMessage::system("s1"),
			ChatMessage::system("s2"),
			ChatMessage::user("hi"),
			ChatMessage::assistant("yo"),
		];
		let body = build_body("gemini-2.5-flash", &msgs, &sample_tools(), Some(64), true);
		assert_eq!(body["systemInstruction"]["parts"][0]["text"], "s1\n\ns2");
		assert_eq!(
			body["contents"],
			serde_json::json!([
				{"role": "user", "parts": [{"text": "hi"}]},
				{"role": "model", "parts": [{"text": "yo"}]},
			])
		);
		assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
		assert_eq!(body["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
		let decl = &body["tools"][0]["functionDeclarations"][0];
		assert_eq!(decl["name"], "read_file");
		assert_eq!(decl["parametersJsonSchema"]["type"], "object");

		// 思考非対応モデル・ツール無し・上限無しなら余計なキーを付けない
		let body = build_body("gemini-2.0-flash", &msgs, &[], None, true);
		assert!(body.get("generationConfig").is_none());
		assert!(body.get("tools").is_none());
	}

	#[test]
	fn tool_round_trip_keeps_signature_and_ids() {
		let resp = serde_json::json!({
			"candidates": [{"content": {"role": "model", "parts": [
				{"text": "考え中", "thought": true},
				{"text": "読むで"},
				{"functionCall": {"name": "read_file", "args": {"path": "a"}, "id": "fc1"}, "thoughtSignature": "SIG"},
				{"functionCall": {"name": "read_file", "args": {"path": "b"}}},
			]}, "finishReason": "STOP"}],
			"usageMetadata": {
				"promptTokenCount": 100, "candidatesTokenCount": 20, "thoughtsTokenCount": 30,
				"cachedContentTokenCount": 40, "totalTokenCount": 150
			},
			"responseId": "r1"
		});
		let c = parse_completion(&resp);
		assert_eq!(c.text, "読むで");
		assert_eq!(c.thinking.as_deref(), Some("考え中"));
		assert_eq!(c.tool_calls.len(), 2);
		assert_eq!(c.tool_calls[0].id, "fc1");
		assert_eq!(
			c.tool_calls[0].extra,
			serde_json::json!({"thoughtSignature": "SIG", "id": "fc1"})
		);
		assert_eq!(c.tool_calls[1].id, "");
		assert!(c.tool_calls[1].extra.is_null());
		let u = c.usage.unwrap();
		assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens), (100, 50, 150));
		assert_eq!((u.cached_tokens, u.reasoning_tokens), (Some(40), Some(30)));
		assert_eq!(u.request_id.as_deref(), Some("r1"));

		// エージェントループ相当: 空IDを補って履歴に積み、結果を返す
		let mut calls = c.tool_calls;
		calls[1].id = "call_1_1".into();
		let history = vec![
			ChatMessage::user("go"),
			ChatMessage::assistant_tool_calls("読むで", calls),
			ChatMessage::tool_result("fc1", "A"),
			ChatMessage::tool_result("call_1_1", "Error: nope"),
		];
		let (_, contents) = build_contents(&history);
		assert_eq!(contents.len(), 3);
		let model_parts = contents[1]["parts"].as_array().unwrap();
		assert_eq!(model_parts[0], serde_json::json!({"text": "読むで"}));
		assert_eq!(
			model_parts[1],
			serde_json::json!({"functionCall": {"name": "read_file", "args": {"path": "a"}, "id": "fc1"}, "thoughtSignature": "SIG"})
		);
		// 署名は先頭だけ。API が返していない ID は送らない
		assert_eq!(
			model_parts[2],
			serde_json::json!({"functionCall": {"name": "read_file", "args": {"path": "b"}}})
		);
		let results = contents[2]["parts"].as_array().unwrap();
		assert_eq!(contents[2]["role"], "user");
		assert_eq!(
			results[0],
			serde_json::json!({"functionResponse": {"name": "read_file", "response": {"result": "A"}, "id": "fc1"}})
		);
		assert_eq!(
			results[1],
			serde_json::json!({"functionResponse": {"name": "read_file", "response": {"error": "nope"}}})
		);
	}

	#[test]
	fn foreign_history_gets_skip_signature() {
		let calls = vec![
			ToolCall::new("c1", "read_file", serde_json::json!({"path": "a"})),
			ToolCall::new("c2", "read_file", serde_json::json!("{broken")),
		];
		let (_, contents) = build_contents(&[ChatMessage::assistant_tool_calls("", calls)]);
		let parts = contents[0]["parts"].as_array().unwrap();
		assert_eq!(parts.len(), 2);
		assert_eq!(parts[0]["thoughtSignature"], SKIP_SIGNATURE);
		assert!(parts[1].get("thoughtSignature").is_none());
		// 引数がオブジェクトでなければ空オブジェクトに
		assert_eq!(parts[1]["functionCall"]["args"], serde_json::json!({}));
	}

	#[test]
	fn usage_without_thoughts_and_blocking() {
		let v = serde_json::json!({"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2}});
		let u = parse_usage(&v).unwrap();
		assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens), (5, 2, 7));
		assert_eq!(u.reasoning_tokens, None);
		assert!(parse_usage(&serde_json::json!({})).is_none());

		assert_eq!(
			blocked_reason(&serde_json::json!({"candidates": [{"finishReason": "STOP"}]})),
			None
		);
		assert_eq!(
			blocked_reason(&serde_json::json!({"candidates": [{"finishReason": "SAFETY"}]})).as_deref(),
			Some("finishReason=SAFETY")
		);
		assert_eq!(
			blocked_reason(&serde_json::json!({"promptFeedback": {"blockReason": "OTHER"}})).as_deref(),
			Some("prompt blocked: OTHER")
		);
	}

	#[test]
	fn error_body() {
		let (code, msg) = parse_error_body(
			r#"{"error": {"code": 400, "message": "API key not valid.", "status": "INVALID_ARGUMENT"}}"#,
		);
		assert_eq!(code.as_deref(), Some("INVALID_ARGUMENT"));
		assert_eq!(
			hint_for_status(400, msg),
			"API key not valid. (APIキーを確認: GEMINI_API_KEY)"
		);
		assert_eq!(parse_error_body("oops"), (None, "oops".into()));
	}
}
