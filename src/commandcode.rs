//! Command Code Provider API の実装.
//!
//! Docs: https://commandcode.ai/docs/provider
//! Base:  https://api.commandcode.ai/provider/v1
//!
//! - `claude-*`            -> POST /messages (Anthropic Messages)
//! - それ以外              -> POST /chat/completions (OpenAI Chat Completions)
//! - `supported_endpoints` に `/responses` を含む -> POST /responses も可
//!
//! [`CommandCodeClient`] は [`crate::llm::LlmClient`] を実装するため、
//! `Arc<dyn LlmClient>` として差し替え可能。

use futures_util::StreamExt;

use crate::llm::LlmClient;
// 後方互換: 既存の `commandcode::{ChatMessage, Error, ModelInfo, Result}` importを壊さない
pub use crate::llm::{ChatMessage, Error, ModelInfo, Result};

pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
const MODELS_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// Auth resolution (pi-commandcode-provider と同じ優先順位)
// ---------------------------------------------------------------------------

fn api_key_from_json(value: &serde_json::Value) -> Option<String> {
	// {"apiKey": "user_..."}
	if let Some(s) = value.get("apiKey").and_then(|v| v.as_str())
		&& !s.is_empty()
	{
		return Some(s.to_string());
	}
	for key in ["commandcode", "command-code"] {
		let entry = value.get(key)?;
		// "commandcode": "user_..."
		if let Some(s) = entry.as_str()
			&& !s.is_empty()
		{
			return Some(s.to_string());
		}
		// {"type":"api","key":"..."} / {"type":"oauth","access":"..."}
		if let Some(s) = entry.get("key").and_then(|v| v.as_str())
			&& !s.is_empty()
		{
			return Some(s.to_string());
		}
		if let Some(s) = entry.get("access").and_then(|v| v.as_str())
			&& !s.is_empty()
		{
			return Some(s.to_string());
		}
	}
	None
}

fn read_key_file(path: &std::path::Path) -> Option<String> {
	let text = std::fs::read_to_string(path).ok()?;
	let v: serde_json::Value = serde_json::from_str(&text).ok()?;
	api_key_from_json(&v)
}

/// 環境変数 → 認証ファイルの順でキーを探す。
pub fn load_api_key() -> Option<String> {
	for env_key in ["COMMAND_CODE_API_KEY", "COMMANDCODE_API_KEY"] {
		if let Ok(v) = std::env::var(env_key)
			&& !v.trim().is_empty()
		{
			return Some(v.trim().to_string());
		}
	}
	let home = std::env::var("HOME").ok()?;
	let home = std::path::PathBuf::from(home);

	// pi は PI_CODING_AGENT_DIR を尊重する
	let pi_dir = std::env::var("PI_CODING_AGENT_DIR")
		.map(std::path::PathBuf::from)
		.unwrap_or_else(|_| home.join(".pi").join("agent"));

	let candidates = [
		home.join(".commandcode").join("auth.json"),
		pi_dir.join("auth.json"),
		home.join(".omp").join("agent").join("auth.json"),
	];
	candidates.iter().find_map(|p| read_key_file(p))
}

fn zdr_enabled() -> bool {
	std::env::var("CMD_ZDR").as_deref() == Ok("1") || std::env::var("COMMANDCODE_ZDR").as_deref() == Ok("1")
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CommandCodeClient {
	http: reqwest::Client,
	base_url: String,
	api_key: String,
	zdr: bool,
}

impl CommandCodeClient {
	pub fn new(api_key: impl Into<String>, base_url: impl Into<String>, zdr: bool) -> Self {
		let http = reqwest::Client::builder()
			.timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
			.build()
			.expect("reqwest client build");
		Self {
			http,
			base_url: base_url.into().trim_end_matches('/').to_string(),
			api_key: api_key.into(),
			zdr,
		}
	}

	pub fn from_env() -> Result<Self> {
		let api_key = load_api_key().ok_or(Error::AuthMissing)?;
		let base_url = std::env::var("COMMANDCODE_API_BASE")
			.ok()
			.filter(|s| !s.trim().is_empty())
			.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
		Ok(Self::new(api_key, base_url, zdr_enabled()))
	}

	fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
		let req = req.bearer_auth(&self.api_key);
		if self.zdr { req.header("x-cmd-zdr", "1") } else { req }
	}

	async fn check_error(&self, res: reqwest::Response) -> Result<reqwest::Response> {
		if res.status().is_success() {
			return Ok(res);
		}
		let status = res.status().as_u16();
		let body = res.text().await.unwrap_or_default();
		let (code, message) = parse_error_body(&body);
		Err(Error::Api {
			status,
			code,
			message: hint_for_status(status, message),
		})
	}
}

#[async_trait::async_trait]
impl LlmClient for CommandCodeClient {
	fn name(&self) -> &'static str {
		"commandcode"
	}

	// -- GET /models (認証不要・Authorizationを付けると403になる) --------------
	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let response = self
			.http
			.get(format!("{}/models", self.base_url))
			.header("accept", "application/json")
			.timeout(std::time::Duration::from_secs(MODELS_TIMEOUT_SECS))
			.send()
			.await?;

		let response = self.check_error(response).await?;
		let wrapped: ModelsListResponse = response.json().await?;

		Ok(wrapped
			.data
			.into_iter()
			.map(|m| ModelInfo {
				id: m.id,
				name: m.name,
				context_length: m.context_length,
				supported_endpoints: m.supported_endpoints,
			})
			.collect())
	}

	// -- POST /chat/completions (OpenAI Chat, 非ストリーム) ------------------
	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, max_tokens: Option<u32>) -> Result<String> {
		let mut body = serde_json::json!({
			"model": model,
			"messages": messages,
			"stream": false,
		});
		if let Some(n) = max_tokens {
			body["max_tokens"] = serde_json::json!(n);
		}
		let res = self
			.auth(
				self.http
					.post(format!("{}/chat/completions", self.base_url))
					.json(&body),
			)
			.send()
			.await?;
		let res = self.check_error(res).await?;
		let parsed: ChatCompletionResponse = res.json().await?;
		Ok(parsed
			.choices
			.into_iter()
			.next()
			.and_then(|c| c.message.content)
			.unwrap_or_default())
	}

	// -- POST /chat/completions (ストリーム, SSE) -----------------------------
	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		let body = serde_json::json!({
			"model": model,
			"messages": messages,
			"stream": true,
		});
		let res = self
			.auth(
				self.http
					.post(format!("{}/chat/completions", self.base_url))
					.json(&body),
			)
			.send()
			.await?;
		let res = self.check_error(res).await?;
		let mut stream = res.bytes_stream();
		let mut buf = String::new();
		let mut full = String::new();

		while let Some(chunk) = stream.next().await {
			let chunk = chunk?;
			buf.push_str(&String::from_utf8_lossy(&chunk));
			// SSEは空行区切り
			while let Some(pos) = buf.find("\n\n") {
				let event: String = buf.drain(..pos + 2).collect();
				for line in event.lines() {
					let line = line.trim();
					let Some(data) = line.strip_prefix("data:") else {
						continue;
					};
					let data = data.trim();
					if data == "[DONE]" {
						continue;
					}
					if let Ok(v) = serde_json::from_str::<serde_json::Value>(data)
						&& let Some(delta) = v.pointer("/choices/0/delta/content").and_then(|c| c.as_str())
					{
						full.push_str(delta);
						on_delta(delta);
					}
				}
			}
		}
		Ok(full)
	}

	// -- POST /responses (OpenAI Responses, 非ストリーム) ---------------------
	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		let body = serde_json::json!({
			"model": model,
			"input": input,
			"stream": false,
		});
		let res = self
			.auth(self.http.post(format!("{}/responses", self.base_url)).json(&body))
			.send()
			.await?;
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await?;
		Ok(extract_responses_text(&v))
	}

	// -- POST /messages (Anthropic Messages, Claude系, 非ストリーム) ---------
	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<String> {
		// Anthropic形: systemはトップレベル、messagesは user/assistant のみ
		let mut body = serde_json::json!({
			"model": model,
			"max_tokens": max_tokens,
			"messages": messages.iter().map(|m| serde_json::json!({
				"role": if m.role == "system" { "user" } else { &m.role },
				"content": m.content,
			})).collect::<Vec<_>>(),
		});
		if let Some(s) = system {
			body["system"] = serde_json::json!(s);
		}
		let res = self
			.auth(
				self.http
					.post(format!("{}/messages", self.base_url))
					.header("anthropic-version", "2023-06-01")
					.json(&body),
			)
			.send()
			.await?;
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await?;
		// {"content":[{"type":"text","text":"..."}]}
		let text = v
			.get("content")
			.and_then(|c| c.as_array())
			.map(|arr| {
				arr.iter()
					.filter_map(|b| b.get("text").and_then(|t| t.as_str()))
					.collect::<Vec<_>>()
					.join("")
			})
			.unwrap_or_default();
		Ok(text)
	}
}

// ---------------------------------------------------------------------------
// Types (wire-local)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct ModelsListResponse {
	#[allow(dead_code)]
	object: Option<String>,
	#[serde(default)]
	data: Vec<RawModel>,
}

#[derive(Debug, serde::Deserialize)]
struct RawModel {
	id: String,
	name: String,
	context_length: u64,
	supported_endpoints: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatCompletionResponse {
	#[serde(default)]
	choices: Vec<ChatChoice>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatChoice {
	message: ChatChoiceMessage,
}

#[derive(Debug, serde::Deserialize)]
struct ChatChoiceMessage {
	content: Option<String>,
}

// Responses APIのテキスト抽出: output_text優先、なければ再帰走査
fn extract_responses_text(v: &serde_json::Value) -> String {
	if let Some(s) = v.get("output_text").and_then(|t| t.as_str()) {
		return s.to_string();
	}
	let mut out = String::new();
	fn walk(v: &serde_json::Value, out: &mut String) {
		match v {
			serde_json::Value::Object(map) => {
				let is_text = map.get("type").and_then(|t| t.as_str()) == Some("output_text");
				if is_text && let Some(s) = map.get("text").and_then(|t| t.as_str()) {
					out.push_str(s);
				}
				for val in map.values() {
					walk(val, out);
				}
			}
			serde_json::Value::Array(arr) => {
				for val in arr {
					walk(val, out);
				}
			}
			_ => {}
		}
	}
	walk(v, &mut out);
	out
}

fn parse_error_body(body: &str) -> (Option<String>, String) {
	if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
		// OpenAI形: {"error":{"code":"...","message":"..."}}
		if let Some(err) = v.get("error") {
			let code = err.get("code").and_then(|c| c.as_str()).map(|s| s.to_string());
			let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or(body).to_string();
			return (code, msg);
		}
		// Anthropic形: {"type":"...","error":{...}}
		let code = v.get("type").and_then(|t| t.as_str()).map(|s| s.to_string());
		if code.is_some() {
			return (code, body.to_string());
		}
	}
	(None, body.chars().take(500).collect())
}

fn hint_for_status(status: u16, message: String) -> String {
	let hint = match status {
		401 => " (認証失敗: Bearerトークンを確認。`pi`で/loginし直し)",
		403 if message.contains("upgrade_required") => " (GoプランはAPI不可。GOAT以上にアップグレード)",
		400 if message.contains("unsupported_model") => " (モデルIDを確認: GET /models)",
		400 => " (モデルとendpointの組み合わせを確認: Claudeは/messagesのみ)",
		429 => " (レート制限: backoffして再試行)",
		502 => " (一時的な上流障害: 他モデルで再試行。課金なし)",
		_ => "",
	};
	format!("{message}{hint}")
}
