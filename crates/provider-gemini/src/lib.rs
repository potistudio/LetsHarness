//! Google Gemini API (Generative Language API) の実装.
//!
//! Docs: https://ai.google.dev/api
//! Base: https://generativelanguage.googleapis.com/v1beta
//!
//! - 一覧        : `GET  /models` (ページング)
//! - 非ストリーム : `POST /models/{model}:generateContent`
//! - ストリーム   : `POST /models/{model}:streamGenerateContent?alt=sse`
//!
//! Gemini には chat/completions・responses・messages の区別が無いため、
//! [`Provider`] のどの wire 系メソッドも `generateContent` に寄せる。
//! アカウント単位の残量を返す API は無いので [`Provider::quota`] は未対応 (既定実装)。
//!
//! 思考過程は `thinkingConfig.includeThoughts` で要約を受け取る (思考対応モデルのみ)。
//! ツール呼び出しの `thoughtSignature` は [`ToolCall::extra`](letsharness_kernel::ToolCall)
//! に保持して次ターンで送り返す (Gemini 3 は欠けると 400)。

mod plugin;

pub use plugin::GeminiPlugin;

mod wire;

use futures_util::StreamExt;

use letsharness_kernel::{ChatMessage, ChatOutput, Completion, Error, ModelInfo, Provider, Result, ToolSpec};

const PLUGIN: &str = "gemini";

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const MODELS_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 300;
const USER_AGENT: &str = "letsharness/0.1";
/// `GET /models` の1ページあたり件数 (API上限は1000)。
const MODELS_PAGE_SIZE: u32 = 1000;

fn transport(e: reqwest::Error) -> Error {
	Error::plugin(PLUGIN, e)
}

// -----------------------------------------------------------------------------
// Auth / Config
// -----------------------------------------------------------------------------

/// APIキーを探す。優先順位: `GEMINI_API_KEY` > `GOOGLE_API_KEY` (Google公式SDKと同じ)。
pub fn load_api_key() -> Option<String> {
	["GEMINI_API_KEY", "GOOGLE_API_KEY"].into_iter().find_map(|env| {
		std::env::var(env)
			.ok()
			.map(|v| v.trim().to_string())
			.filter(|v| !v.is_empty())
	})
}

/// ベースURLの上書き (`GEMINI_BASE_URL`)。プロキシ・互換サーバ用。
fn base_url_override() -> Option<String> {
	std::env::var("GEMINI_BASE_URL")
		.ok()
		.map(|v| v.trim().trim_end_matches('/').to_string())
		.filter(|v| !v.is_empty())
}

// -----------------------------------------------------------------------------
// Provider Implementation
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GeminiProvider {
	http_client: reqwest::Client,
	base_url: String,
	api_key: String,
	/// 思考対応モデルで思考過程の要約を要求する。
	include_thoughts: bool,
}

impl GeminiProvider {
	pub fn try_new(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
		let http_client = reqwest::Client::builder()
			.user_agent(USER_AGENT)
			.timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
			.build()
			.map_err(transport)?;
		Ok(Self {
			http_client,
			base_url: base_url.into().trim_end_matches('/').to_string(),
			api_key: api_key.into(),
			include_thoughts: true,
		})
	}

	/// 思考過程の要約を要求するか (既定 `true`)。
	pub fn with_thoughts(mut self, enabled: bool) -> Self {
		self.include_thoughts = enabled;
		self
	}

	pub fn from_env() -> Result<Self> {
		let api_key = load_api_key()
			.ok_or_else(|| Error::Config("APIキーが見つかりません。GEMINI_API_KEY を設定してください".into()))?;
		Self::try_new(api_key, base_url_override().unwrap_or_else(|| DEFAULT_BASE_URL.into()))
	}

	fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
		// クエリ (`?key=`) だとURLごとログに残るのでヘッダで渡す。
		req.header("x-goog-api-key", &self.api_key)
	}

	fn method_url(&self, model: &str, method: &str) -> String {
		format!("{}/{}:{method}", self.base_url, wire::model_resource(model))
	}

	async fn check_error(&self, res: reqwest::Response) -> Result<reqwest::Response> {
		if res.status().is_success() {
			return Ok(res);
		}
		let status = res.status().as_u16();
		let body = res.text().await.unwrap_or_default();
		let (code, message) = wire::parse_error_body(&body);
		Err(Error::Api {
			status,
			code,
			message: wire::hint_for_status(status, message),
		})
	}

	/// `generateContent` 1回。思考過程の要約は `include_thoughts` に従う。
	async fn generate(
		&self,
		model: &str,
		messages: &[ChatMessage],
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		let started = std::time::Instant::now();
		let body = wire::build_body(model, messages, tools, max_tokens, self.include_thoughts);
		let res = self
			.auth(
				self.http_client
					.post(self.method_url(model, "generateContent"))
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await.map_err(transport)?;
		let mut completion = wire::parse_completion(&v);
		if completion.text.is_empty()
			&& completion.tool_calls.is_empty()
			&& let Some(reason) = wire::blocked_reason(&v)
		{
			return Err(Error::plugin(PLUGIN, format!("応答が空です ({reason})")));
		}
		if let Some(u) = completion.usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
		}
		Ok(completion)
	}
}

#[async_trait::async_trait]
impl Provider for GeminiProvider {
	fn name(&self) -> &'static str {
		PLUGIN
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let mut models = Vec::new();
		let mut page_token: Option<String> = None;
		loop {
			let mut req = self
				.http_client
				.get(format!("{}/models", self.base_url))
				.query(&[("pageSize", MODELS_PAGE_SIZE.to_string())])
				.timeout(std::time::Duration::from_secs(MODELS_TIMEOUT_SECS));
			if let Some(t) = &page_token {
				req = req.query(&[("pageToken", t)]);
			}
			let res = self.auth(req).send().await.map_err(transport)?;
			let res = self.check_error(res).await?;
			let v: serde_json::Value = res.json().await.map_err(transport)?;
			let page = v
				.get("models")
				.and_then(|m| m.as_array())
				.map(Vec::as_slice)
				.unwrap_or(&[]);
			models.extend(page.iter().filter_map(model_info));
			page_token = v
				.get("nextPageToken")
				.and_then(|t| t.as_str())
				.filter(|t| !t.is_empty())
				.map(str::to_string);
			if page_token.is_none() {
				return Ok(models);
			}
		}
	}

	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, max_tokens: Option<u32>) -> Result<String> {
		Ok(self.chat_with_usage(model, messages, max_tokens).await?.text)
	}

	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		let c = self.generate(model, &messages, &[], max_tokens).await?;
		Ok(ChatOutput {
			text: c.text,
			thinking: c.thinking,
			usage: c.usage,
		})
	}

	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		Ok(self
			.chat_stream_with_usage(model, messages, on_delta, on_thinking)
			.await?
			.text)
	}

	// -- POST :streamGenerateContent?alt=sse -----------------------------------
	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let started = std::time::Instant::now();
		let body = wire::build_body(model, &messages, &[], None, self.include_thoughts);
		let res = self
			.auth(
				self.http_client
					.post(self.method_url(model, "streamGenerateContent"))
					.query(&[("alt", "sse")])
					.json(&body),
			)
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		let mut stream = res.bytes_stream();
		let mut buf = String::new();
		let mut full = String::new();
		let mut thinking = String::new();
		let mut usage = None;
		let mut ttft_ms: Option<u64> = None;
		let mut blocked: Option<String> = None;

		while let Some(chunk) = stream.next().await {
			let chunk = chunk.map_err(transport)?;
			// Gemini の SSE は CRLF 区切り。JSON 本体に生の CR は現れないので落としてよい。
			buf.extend(String::from_utf8_lossy(&chunk).chars().filter(|&c| c != '\r'));
			while let Some(pos) = buf.find("\n\n") {
				let event: String = buf.drain(..pos + 2).collect();
				for line in event.lines() {
					let Some(data) = line.trim().strip_prefix("data:") else {
						continue;
					};
					let Ok(v) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
						continue;
					};
					if let Some(u) = wire::parse_usage(&v) {
						usage = Some(u);
					}
					if let Some(r) = wire::blocked_reason(&v) {
						blocked = Some(r);
					}
					let p = wire::parse_parts(&v);
					if ttft_ms.is_none() && !(p.thinking.is_empty() && p.text.is_empty()) {
						ttft_ms = Some(started.elapsed().as_millis() as u64);
					}
					if !p.thinking.is_empty() {
						thinking.push_str(&p.thinking);
						on_thinking(&p.thinking);
					}
					if !p.text.is_empty() {
						full.push_str(&p.text);
						on_delta(&p.text);
					}
				}
			}
		}
		if full.is_empty()
			&& let Some(reason) = blocked
		{
			return Err(Error::plugin(PLUGIN, format!("応答が空です ({reason})")));
		}
		if let Some(u) = usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
			u.ttft_ms = ttft_ms;
		}
		Ok(ChatOutput {
			text: full,
			thinking: ChatOutput::normalized_thinking(Some(thinking)),
			usage,
		})
	}

	async fn chat_with_tools(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		self.generate(model, &messages, tools, max_tokens).await
	}

	// Gemini に Responses / Messages の区別は無いので generateContent に寄せる。
	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		Ok(self.responses_with_usage(model, input).await?.text)
	}

	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		self.chat_with_usage(model, vec![ChatMessage::user(input)], None).await
	}

	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<String> {
		Ok(self
			.messages_with_usage(model, messages, system, max_tokens)
			.await?
			.text)
	}

	async fn messages_with_usage(
		&self,
		model: &str,
		mut messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<ChatOutput> {
		// 呼び出し側が system を messages にも入れている場合は二重にしない。
		if let Some(s) = system
			&& !messages.iter().any(|m| m.role == "system")
		{
			messages.insert(0, ChatMessage::system(s));
		}
		self.chat_with_usage(model, messages, Some(max_tokens)).await
	}
}

/// `GET /models` の1件を [`ModelInfo`] に。`supported_endpoints` には
/// `supportedGenerationMethods` (`generateContent` など) をそのまま入れる。
fn model_info(m: &serde_json::Value) -> Option<ModelInfo> {
	let name = m.get("name")?.as_str()?;
	let id = wire::model_id(name).to_string();
	Some(ModelInfo {
		name: m
			.get("displayName")
			.and_then(|d| d.as_str())
			.map(str::to_string)
			.unwrap_or_else(|| id.clone()),
		id,
		context_length: m.get("inputTokenLimit").and_then(|n| n.as_u64()).unwrap_or(0),
		supported_endpoints: m.get("supportedGenerationMethods").and_then(|s| {
			s.as_array()
				.map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
		}),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn model_info_maps_catalog_entry() {
		let m = model_info(&serde_json::json!({
			"name": "models/gemini-2.5-flash",
			"displayName": "Gemini 2.5 Flash",
			"inputTokenLimit": 1048576,
			"supportedGenerationMethods": ["generateContent", "countTokens"]
		}))
		.unwrap();
		assert_eq!(m.id, "gemini-2.5-flash");
		assert_eq!(m.name, "Gemini 2.5 Flash");
		assert_eq!(m.context_length, 1_048_576);
		assert_eq!(m.supported_endpoints.unwrap(), ["generateContent", "countTokens"]);
		assert!(model_info(&serde_json::json!({})).is_none());
	}

	#[test]
	fn method_url_builds_resource_path() {
		let p = GeminiProvider::try_new("k", "https://example.com/v1beta/").unwrap();
		assert_eq!(
			p.method_url("gemini-2.5-pro", "generateContent"),
			"https://example.com/v1beta/models/gemini-2.5-pro:generateContent"
		);
	}
}
