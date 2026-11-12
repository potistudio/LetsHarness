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
//!
//! Usage / Quota の経路:
//! - リクエスト単位の [`Usage`](letsharness_kernel::Usage) は各レスポンスの
//!   `usage` オブジェクトから正規化する (3wireで名前が違うため
//!   `input_tokens`/`output_tokens` 系も受け付ける)。
//! - アカウント単位の [`Quota`](letsharness_kernel::Quota) は CLI と同じ
//!   `/alpha/billing/credits` (+ subscriptions / usage/summary) から取る。
//!   `/provider/v1/*` に課金面は無い (叩くと404)。
//! - 月次capはAPIが返さない。CLIと同じくプランIDから [`plan_monthly_credits`]
//!   で引き、`max(cap, monthlyCredits)` を実効capとする。

use futures_util::StreamExt;

use letsharness_kernel::{ChatMessage, ChatOutput, Error, ModelInfo, Provider, Quota, QuotaWindow, Result, Usage};

const PLUGIN: &str = "commandcode";

/// reqwest のエラーを kernel の [`Error::Plugin`] に包む (kernel は reqwest を知らない)。
fn transport(e: reqwest::Error) -> Error {
	Error::plugin(PLUGIN, e)
}

pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai/provider/v1";
pub const DEFAULT_API_BASE: &str = "https://api.commandcode.ai";
const MODELS_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT_SECS: u64 = 120;
const USER_AGENT: &str = "letsharness/0.1";

/// プランID -> 月次クレジット (USD)。APIに上限を返す口が無いため、
/// command-code CLI 1.88.5 (`dist/cli.mjs` の `getPlanInfo`) の定義を写したもの。
/// サーバ側の改定に追従しないので、未知/変更時は `None` に倒す。
const PLAN_MONTHLY_CREDITS: &[(&str, f64)] = &[
	("individual-go", 10.0),
	("individual-go-v1", 10.0),
	("individual-goat", 70.0),
	("individual-pro", 30.0),
	("individual-pro-v1", 80.0),
	("individual-provider", 15.0),
	("individual-max", 150.0),
	("individual-ultra", 300.0),
	("teams-pro", 40.0),
];

/// CLI同様: 小文字化・`_`→`-` 正規化の上、最長一致のprefixでプランを引く。
pub fn plan_monthly_credits(plan_id: &str) -> Option<f64> {
	let id = plan_id.to_ascii_lowercase().replace('_', "-");
	PLAN_MONTHLY_CREDITS
		.iter()
		.filter(|(k, _)| id.starts_with(k))
		.max_by_key(|(k, _)| k.len())
		.map(|(_, v)| *v)
}

// -----------------------------------------------------------------------------
// Auth
// -----------------------------------------------------------------------------

/// APIキーを探す。優先順位: `COMMAND_CODE_API_KEY` > `COMMANDCODE_API_KEY` >
/// `~/.commandcode/auth.json` (`commandcode.key` / `apiKey` / `api_key`)。
pub fn load_api_key() -> Option<String> {
	for env in ["COMMAND_CODE_API_KEY", "COMMANDCODE_API_KEY"] {
		if let Ok(value) = std::env::var(env)
			&& !value.trim().is_empty()
		{
			return Some(value.trim().to_string());
		}
	}
	read_auth_file_key()
}

/// `~/.commandcode/auth.json` (CLIの `login` が書く) からキーを読む。
fn read_auth_file_key() -> Option<String> {
	let path = dirs_home()?.join(".commandcode").join("auth.json");
	let raw = std::fs::read_to_string(path).ok()?;
	let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
	// {"commandcode": {"key": "user_..."}} 形式が正規。旧形式のトップレベルも許容。
	for pointer in [
		"/commandcode/key",
		"/commandcode/apiKey",
		"/commandcode/api_key",
		"/apiKey",
		"/api_key",
	] {
		if let Some(s) = v.pointer(pointer).and_then(|x| x.as_str())
			&& !s.trim().is_empty()
		{
			return Some(s.trim().to_string());
		}
	}
	None
}

fn dirs_home() -> Option<std::path::PathBuf> {
	std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// APIベース (`https://api.commandcode.ai` 部分) の上書き。
/// `COMMANDCODE_API_URL` > `COMMAND_CODE_API_URL` > 既定。
fn api_base_override() -> Option<String> {
	for env in ["COMMANDCODE_API_URL", "COMMAND_CODE_API_URL"] {
		if let Ok(value) = std::env::var(env) {
			let trimmed = value.trim().trim_end_matches('/').to_string();
			if !trimmed.is_empty() {
				return Some(trimmed);
			}
		}
	}
	None
}

fn zdr_enabled() -> bool {
	std::env::var("CMD_ZDR").as_deref() == Ok("1") || std::env::var("COMMANDCODE_ZDR").as_deref() == Ok("1")
}

// -----------------------------------------------------------------------------
// Provider Implementation
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
			Error::Config("APIキーが見つかりません。COMMAND_CODE_API_KEY を設定するか `cmd login` してください".into())
		})?;
		let base_url = api_base_override()
			.map(|b| format!("{b}/provider/v1"))
			.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
		let provider = Self::try_new(api_key, base_url)?.with_zdr(zdr_enabled());

		Ok(provider)
	}

	/// `/alpha/...` (CLIと同じ課金面) のベース。`base_url` の末尾
	/// `/provider/v1` を剥がす。剥がせない場合は既定ホストを使う。
	fn alpha_base(&self) -> String {
		if let Some(o) = api_base_override() {
			return format!("{o}/alpha");
		}
		let stripped = self
			.base_url
			.strip_suffix("/provider/v1")
			.or_else(|| self.base_url.strip_suffix("/provider/v1/"))
			.or_else(|| self.base_url.strip_suffix("/provider"))
			.unwrap_or(&self.base_url);
		// base_url自体が既にAPIホスト直下の場合はそのまま /alpha を足す。
		// ただしテスト用の localhost 等もそのまま活かす。
		if stripped == self.base_url && !self.base_url.ends_with("/alpha") {
			// 未知のベース形: ホスト部分だけ抜き出せなければ既定を使う。
			if let Ok(url) = url_host(&self.base_url) {
				return format!("{url}/alpha");
			}
			return format!("{DEFAULT_API_BASE}/alpha");
		}
		format!("{stripped}/alpha")
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

	async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
		let res = self
			.auth(self.http_client.get(url).header("accept", "application/json"))
			.send()
			.await
			.map_err(transport)?;
		let res = self.check_error(res).await?;
		res.json().await.map_err(transport)
	}
}

/// `https://host/prefix...` から `https://host` を抜く。失敗時は Err。
fn url_host(url: &str) -> std::result::Result<String, ()> {
	let scheme_end = url.find("://").ok_or(())?;
	let rest = &url[scheme_end + 3..];
	let host_end = rest.find('/').unwrap_or(rest.len());
	Ok(format!("{}{}", &url[..scheme_end + 3], &rest[..host_end]))
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

	// -- Quota: GET /alpha/billing/credits (+ subscriptions / usage/summary) --
	async fn quota(&self) -> Result<Quota> {
		let alpha = self.alpha_base();
		// org所属時は ?orgId= が必要。失敗時はクエリ無しで続行せずエラーにする
		// (whoami自体が認証の成否を兼ねる)。
		let whoami = self.get_json(&format!("{alpha}/whoami?limits=1")).await?;
		let org = whoami
			.pointer("/org/id")
			.and_then(|v| v.as_str())
			.map(|s| s.to_string());
		let query = org.as_deref().map(|id| format!("?orgId={id}")).unwrap_or_default();

		let credits = self.get_json(&format!("{alpha}/billing/credits{query}")).await?;
		// サブ経路は任意: 壊れても残高・枠は返す (OpenUsageと同じ方針)。
		let subscription = self
			.get_json(&format!("{alpha}/billing/subscriptions{query}"))
			.await
			.ok();
		let summary = self.get_json(&format!("{alpha}/usage/summary{query}")).await.ok();

		Ok(compose_quota(credits, subscription, summary))
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
		let started = std::time::Instant::now();
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
		let request_id = header_request_id(&res);
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await.map_err(transport)?;
		let text = v
			.pointer("/choices/0/message/content")
			.and_then(|c| c.as_str())
			.unwrap_or_default()
			.to_string();
		let mut usage = v.get("usage").and_then(Usage::from_value);
		if let Some(u) = usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
			u.request_id = request_id.or_else(|| v.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()));
		}
		Ok(ChatOutput { text, usage })
	}

	// -- POST /chat/completions (ストリーム, SSE) -----------------------------
	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		Ok(self.chat_stream_with_usage(model, messages, on_delta).await?.text)
	}

	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let started = std::time::Instant::now();
		let body = serde_json::json!({
			"model": model,
			"messages": messages,
			"stream": true,
			"stream_options": {"include_usage": true},
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
		let request_id = header_request_id(&res);
		let res = self.check_error(res).await?;
		let mut stream = res.bytes_stream();
		let mut buf = String::new();
		let mut full = String::new();
		let mut usage: Option<Usage> = None;

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
					let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
						continue;
					};
					if let Some(u) = v.get("usage").and_then(Usage::from_value) {
						usage = Some(u);
					}
					if let Some(delta) = v.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
						full.push_str(delta);
						on_delta(delta);
					}
				}
			}
		}
		if let Some(u) = usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
			u.request_id = request_id;
		}
		Ok(ChatOutput { text: full, usage })
	}

	// -- POST /responses (OpenAI Responses, 非ストリーム) ---------------------
	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		Ok(self.responses_with_usage(model, input).await?.text)
	}

	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		let started = std::time::Instant::now();
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
		let request_id = header_request_id(&res);
		let res = self.check_error(res).await?;
		let v: serde_json::Value = res.json().await.map_err(transport)?;
		let text = extract_responses_text(&v);
		let mut usage = v.get("usage").and_then(Usage::from_value);
		if let Some(u) = usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
			u.request_id = request_id.or_else(|| v.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()));
		}
		Ok(ChatOutput { text, usage })
	}

	// -- POST /messages (Anthropic Messages, Claude系, 非ストリーム) ---------
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
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<ChatOutput> {
		let started = std::time::Instant::now();
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
		let request_id = header_request_id(&res);
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
		let mut usage = v.get("usage").and_then(Usage::from_value);
		if let Some(u) = usage.as_mut() {
			u.latency_ms = Some(started.elapsed().as_millis() as u64);
			u.request_id = request_id.or_else(|| v.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()));
		}
		Ok(ChatOutput { text, usage })
	}
}

fn header_request_id(res: &reqwest::Response) -> Option<String> {
	res.headers()
		.get("x-trace-id")
		.and_then(|v| v.to_str().ok())
		.map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Quota 合成
// ---------------------------------------------------------------------------

fn as_f64(v: &serde_json::Value) -> Option<f64> {
	if let Some(n) = v.as_f64() {
		return Some(n);
	}
	if let Some(n) = v.as_u64() {
		return Some(n as f64);
	}
	v.as_i64().map(|n| n as f64)
}

fn parse_window(v: Option<&serde_json::Value>) -> Option<QuotaWindow> {
	let v = v?;
	let used = v.get("used").and_then(as_f64)?;
	let cap = v.get("cap").and_then(as_f64)?;
	if cap <= 0.0 {
		return None;
	}
	let reset_at_ms = v
		.get("resetAt")
		.and_then(|r| r.as_i64().or_else(|| r.as_u64().map(|n| n as i64)));
	let exceeded = v.get("exceeded").and_then(|e| e.as_bool()).unwrap_or(false);
	Some(QuotaWindow::new(used, cap, reset_at_ms, exceeded))
}

/// `/alpha/*` の3レスポンスを [`Quota`] に正規化する。
fn compose_quota(
	credits: serde_json::Value,
	subscription: Option<serde_json::Value>,
	summary: Option<serde_json::Value>,
) -> Quota {
	let monthly = credits.pointer("/credits/monthlyCredits").and_then(as_f64);
	let purchased = credits.pointer("/credits/purchasedCredits").and_then(as_f64);
	let free = credits.pointer("/credits/freeCredits").and_then(as_f64);
	let balance = match (monthly, purchased, free) {
		(Some(m), Some(p), Some(f)) => Some(m + p + f),
		_ => [monthly, purchased, free].into_iter().flatten().reduce(|a, b| a + b),
	};
	let five_hour = parse_window(credits.pointer("/windowLimits/fiveHour"));
	let weekly = parse_window(credits.pointer("/windowLimits/weekly"));
	let (plan, status) = subscription
		.as_ref()
		.map(|s| {
			let plan = s
				.pointer("/data/planId")
				.or_else(|| s.pointer("/planId"))
				.and_then(|v| v.as_str())
				.map(|x| x.to_string());
			let status = s
				.pointer("/data/status")
				.or_else(|| s.pointer("/status"))
				.and_then(|v| v.as_str())
				.map(|x| x.to_string());
			(plan, status)
		})
		.unwrap_or((None, None));
	let period_end_ms = subscription
		.as_ref()
		.and_then(|s| {
			s.pointer("/data/currentPeriodEnd")
				.or_else(|| s.pointer("/currentPeriodEnd"))
		})
		.and_then(|v| v.as_str())
		.and_then(parse_iso8601_ms);
	// CLIの projectUsageView と同じ: active時のみプラン定義のcapを採用し、
	// 繰越等で残高がcapを超える場合は残高側をcapとみなす。
	let monthly_window = match (
		status.as_deref(),
		plan.as_deref().and_then(plan_monthly_credits),
		monthly,
	) {
		(Some("active"), Some(cap), Some(rem)) => {
			let cap = cap.max(rem);
			Some(QuotaWindow::new(cap - rem, cap, period_end_ms, rem <= 0.0))
		}
		_ => None,
	};
	let period_spend = summary.as_ref().and_then(|s| s.get("totalCost").and_then(as_f64));
	let period_tokens = summary.as_ref().and_then(|s| {
		s.get("totalTokens")
			.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
	});
	let extra = serde_json::json!({
		"credits": credits,
		"subscription": subscription,
		"summary": summary,
	});
	Quota {
		plan,
		status,
		credit_balance: balance,
		monthly_remaining: monthly,
		purchased_credits: purchased,
		free_credits: free,
		five_hour,
		weekly,
		monthly: monthly_window,
		period_spend,
		period_tokens,
		extra,
	}
}

/// `YYYY-MM-DDTHH:MM:SS(.fff)?(Z|+00:00)` (UTC) を epoch ms に。それ以外は `None`。
fn parse_iso8601_ms(s: &str) -> Option<i64> {
	let (date, time) = s.split_once('T')?;
	let time = time.strip_suffix('Z').or_else(|| time.strip_suffix("+00:00"))?;
	let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
	let (y, m, day) = (d.next()??, d.next()??, d.next()??);
	let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
	let mut t = hms.splitn(3, ':').map(|x| x.parse::<i64>().ok());
	let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
	if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
		return None;
	}
	let ms = if frac.is_empty() {
		0
	} else {
		let f: String = frac.chars().chain("000".chars()).take(3).collect();
		f.parse::<i64>().ok()?
	};
	// Howard Hinnant の days_from_civil
	let y = if m <= 2 { y - 1 } else { y };
	let era = y.div_euclid(400);
	let yoe = y - era * 400;
	let mp = (m + 9) % 12;
	let doy = (153 * mp + 2) / 5 + day - 1;
	let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	let days = era * 146097 + doe - 719468;
	Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
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
		401 => " (認証失敗: Bearerトークンを確認。`cmd login`し直し)",
		403 if message.contains("upgrade_required") => " (GoプランはAPI不可。GOAT以上にアップグレード)",
		400 if message.contains("unsupported_model") => " (モデルIDを確認: GET /models)",
		400 => " (モデルとendpointの組み合わせを確認: Claudeは/messagesのみ)",
		429 => " (レート制限: backoffして再試行)",
		502 => " (一時的な上流障害: 他モデルで再試行。課金なし)",
		_ => "",
	};
	format!("{message}{hint}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn alpha_base_strips_provider_suffix() {
		let p = CommandCodeProvider::try_new("k", DEFAULT_BASE_URL).unwrap();
		// 環境変数の影響を受けないよう直接検証
		assert!(p.base_url.ends_with("/provider/v1"));
		let alpha = p.alpha_base();
		assert!(alpha.ends_with("/alpha"), "{alpha}");
		assert!(!alpha.contains("/provider/"), "{alpha}");
	}

	#[test]
	fn compose_quota_maps_live_shape() {
		let credits = serde_json::json!({
			"credits": {"monthlyCredits": 65.39, "purchasedCredits": 0.0, "freeCredits": 0.0},
			"windowLimits": {
				"fiveHour": {"used": 0.22, "cap": 14.0, "exceeded": false, "resetAt": 1794530518804_i64},
				"weekly": {"used": 4.6, "cap": 35.0, "exceeded": false, "resetAt": 1794932094285_i64}
			}
		});
		let sub = serde_json::json!({"success": true, "data": {
			"planId": "individual-goat", "status": "active",
			"currentPeriodEnd": "2026-12-10T15:25:19.000Z"
		}});
		let summary = serde_json::json!({"totalCost": 4.6, "totalTokens": 99552068_u64});
		let q = compose_quota(credits, Some(sub), Some(summary));
		assert_eq!(q.plan.as_deref(), Some("individual-goat"));
		assert!((q.credit_balance.unwrap() - 65.39).abs() < 1e-9);
		let five = q.five_hour.as_ref().unwrap();
		assert!((five.remaining - (14.0 - 0.22)).abs() < 1e-9);
		assert_eq!(five.reset_at_ms, Some(1794530518804));
		assert_eq!(q.ratelimit_reset(), Some(1794530518804));
		assert_eq!(q.period_tokens, Some(99552068));
		let m = q.monthly.as_ref().unwrap();
		assert_eq!(m.cap, 70.0);
		assert!((m.remaining - 65.39).abs() < 1e-9);
		assert!((m.used - (70.0 - 65.39)).abs() < 1e-9);
		assert_eq!(m.reset_at_ms, Some(1_796_916_319_000));
		assert!((m.used_pct - (70.0 - 65.39) / 70.0 * 100.0).abs() < 1e-9);
		assert!((five.used_pct - 0.22 / 14.0 * 100.0).abs() < 1e-9);
	}

	#[test]
	fn plan_lookup_longest_prefix() {
		assert_eq!(plan_monthly_credits("individual-goat"), Some(70.0));
		assert_eq!(plan_monthly_credits("individual-go"), Some(10.0));
		assert_eq!(plan_monthly_credits("INDIVIDUAL_PRO_V1"), Some(80.0));
		assert_eq!(plan_monthly_credits("individual-pro-2027"), Some(30.0));
		assert_eq!(plan_monthly_credits("enterprise"), None);
	}

	#[test]
	fn monthly_cap_needs_active_and_known_plan() {
		let credits = serde_json::json!({"credits": {"monthlyCredits": 80.0}});
		// 繰越で残高 > cap なら残高をcapに
		let sub = serde_json::json!({"data": {"planId": "individual-goat", "status": "active"}});
		let m = compose_quota(credits.clone(), Some(sub), None).monthly.unwrap();
		assert_eq!((m.cap, m.used, m.remaining), (80.0, 0.0, 80.0));
		let sub = serde_json::json!({"data": {"planId": "individual-goat", "status": "canceled"}});
		assert!(compose_quota(credits.clone(), Some(sub), None).monthly.is_none());
		let sub = serde_json::json!({"data": {"planId": "mystery", "status": "active"}});
		assert!(compose_quota(credits, Some(sub), None).monthly.is_none());
	}

	#[test]
	fn iso8601_parse() {
		assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
		assert_eq!(parse_iso8601_ms("2026-12-10T15:25:19.000Z"), Some(1_796_916_319_000));
		assert_eq!(parse_iso8601_ms("2026-12-10T15:25:19.5+00:00"), Some(1_796_916_319_500));
		assert_eq!(parse_iso8601_ms("2026-12-10T15:25:19+09:00"), None);
		assert_eq!(parse_iso8601_ms("garbage"), None);
	}

	#[test]
	fn usage_parses_three_wires() {
		// chat/completions
		let u = Usage::from_value(&serde_json::json!({
			"prompt_tokens": 138, "completion_tokens": 4, "total_tokens": 142,
			"prompt_tokens_details": {"cached_tokens": 132}
		}))
		.unwrap();
		assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens), (138, 4, 142));
		assert_eq!(u.cached_tokens, Some(132));
		// responses
		let u = Usage::from_value(&serde_json::json!({
			"input_tokens": 138, "output_tokens": 4, "total_tokens": 142,
			"input_tokens_details": {"cached_tokens": 132}
		}))
		.unwrap();
		assert_eq!((u.prompt_tokens, u.completion_tokens), (138, 4));
		// messages (anthropic)
		let u = Usage::from_value(&serde_json::json!({
			"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 8
		}))
		.unwrap();
		assert_eq!(u.cached_tokens, Some(8));
		assert_eq!(u.total_tokens, 15);
	}
}
