//! Command Code Provider API の実装.
//!
//! Docs: https://commandcode.ai/docs/provider
//! Base:  https://api.commandcode.ai/provider/v1
//!
//! - `claude-*`            -> POST /messages (Anthropic Messages)
//! - それ以外              -> POST /chat/completions (OpenAI Chat Completions)
//! - `supported_endpoints` に `/responses` を含む -> POST /responses も可
//!
//! [`CommandCodeProvider`] は [`Provider`] を実装するため、
//! `Arc<dyn Provider>` として差し替え可能。

use futures_util::StreamExt;

use letsharness_kernel::{ChatMessage, Error, ModelInfo, Provider, Result};

const PLUGIN: &str = "commandcode";

/// reqwest のエラーを kernel の [`Error::Plugin`] に包む (kernel は reqwest を知らない)。
fn transport(e: reqwest::Error) -> Error {
	Error::plugin(PLUGIN, e)
}

pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
const MODELS_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;

// -----------------------------------------------------------------------------
// Auth
// -----------------------------------------------------------------------------

/// Finds the API key from the environment variable.
pub fn load_api_key() -> Option<String> {
	if let Ok(value) = std::env::var("COMMAND_CODE_API_KEY")
		&& !value.trim().is_empty()
	{
		return Some(value.trim().to_string());
	}

	None
}

fn zdr_enabled() -> bool {
	std::env::var("CMD_ZDR").as_deref() == Ok("1") || std::env::var("COMMANDCODE_ZDR").as_deref() == Ok("1")
}

// -----------------------------------------------------------------------------
// Client
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CommandCodeProvider {
	/// Client for HTTP requests.
	http_client: reqwest::Client,

	base_url: String,

	api_key: String,
	zdr: bool,
}

impl CommandCodeProvider {
	/// Create a new instance with the given API key and base URL.
	///
	/// # Panics
	/// Panics if the API key is empty or the base URL is invalid.
	pub fn try_new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
		let http_client = reqwest::Client::builder()
			.timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
			.build()
			.map_err(transport)?;

		Ok(Self {
			http_client,
			base_url: base_url.into().trim_end_matches('/').to_string(),
			api_key: api_key.into(),
			zdr: false,
		})
	}

	/// Create a new instance with Zero Data Retention (ZDR) enabled.
	pub fn with_zdr(mut self, enabled: bool) -> Self {
		self.zdr = enabled;
		self
	}

	pub fn from_env() -> Result<Self> {
		let api_key = load_api_key().ok_or_else(|| {
			Error::Config(
				"APIキーが見つかりません。COMMAND_CODE_API_KEY を設定するか `pi` で /login してください".into(),
			)
		})?;
		let provider = Self::try_new(api_key, DEFAULT_BASE_URL.to_string())?.with_zdr(zdr_enabled());

		Ok(provider)
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
impl Provider for CommandCodeProvider {
	fn name(&self) -> &'static str {
		"commandcode"
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let response = self
			.http_client
			.get(format!("{}/models", self.base_url))
			.header("accept", "application/json")
			.timeout(std::time::Duration::from_secs(MODELS_TIMEOUT_SECS))
			.send()
			.await
			.map_err(transport)?;

		let response = self.check_error(response).await?;
		let wrapped: ModelsListResponse = response.json().await.map_err(transport)?;

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
				self.http_client
					.post(format!("{}/chat/completions", self.base_url))
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let parsed: ChatCompletionResponse = res.json().await.map_err(transport)?;
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
				self.http_client
					.post(format!("{}/chat/completions", self.base_url))
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let mut stream = res.bytes_stream();
		let mut buf = String::new();
		let mut full = String::new();

		while let Some(chunk) = stream.next().await {
			let chunk = chunk.map_err(transport)?;
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
			.auth(
				self.http_client
					.post(format!("{}/responses", self.base_url))
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await.map_err(transport)?;
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
				self.http_client
					.post(format!("{}/messages", self.base_url))
					.header("anthropic-version", "2023-06-01")
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await.map_err(transport)?;
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
