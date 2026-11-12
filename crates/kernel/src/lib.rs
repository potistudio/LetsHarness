//! LetsHarness kernel: 拡張点の trait と共有型.
//!
//! bin やハーネス本体はこの crate だけに依存し、
//! 具体的な実装 (`commandcode` / `mock` / 将来の他社API) を
//! `Arc<dyn Provider>` として差し替えられる。
//!
//! この crate は UI・HTTP ライブラリ (`clap` / `reqwest` など) に依存してはならない。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// プラグイン側の任意のエラー. 特定実装の型を kernel に持ち込まないために使う。
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// そのプロバイダでは未対応の操作 (例: quota 未実装).
	#[error("unsupported: {0}")]
	Unsupported(String),
	/// 設定不足・設定不正 (APIキー未設定など)。
	#[error("config: {0}")]
	Config(String),
	#[error("APIエラー status={status} code={code:?} message={message}")]
	Api {
		status: u16,
		code: Option<String>,
		message: String,
	},
	/// プラグイン固有のエラー (transport / io など) を包む。
	#[error("{plugin}: {source}")]
	Plugin {
		plugin: &'static str,
		#[source]
		source: BoxError,
	},
}

impl Error {
	pub fn plugin(plugin: &'static str, source: impl Into<BoxError>) -> Self {
		Self::Plugin {
			plugin,
			source: source.into(),
		}
	}
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
	pub id: String,
	pub name: String,
	pub context_length: u64,
	pub supported_endpoints: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
	pub role: String,
	pub content: String,
}

impl ChatMessage {
	pub fn user(content: impl Into<String>) -> Self {
		Self {
			role: "user".into(),
			content: content.into(),
		}
	}
	pub fn system(content: impl Into<String>) -> Self {
		Self {
			role: "system".into(),
			content: content.into(),
		}
	}
}

// ---------------------------------------------------------------------------
// Usage: リクエスト単位のトークン消費 (レスポンス由来)
// ---------------------------------------------------------------------------

/// 1リクエスト分のトークン使用量。
///
/// Command Code の3wireで名前が割れているため正規化する:
/// - Chat Completions: `prompt_tokens` / `completion_tokens`
/// - Responses: `input_tokens` / `output_tokens`
/// - Messages (Anthropic): `input_tokens` / `output_tokens` +
///   `cache_read_input_tokens`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
	pub prompt_tokens: u64,
	pub completion_tokens: u64,
	pub total_tokens: u64,
	pub cached_tokens: Option<u64>,
	pub reasoning_tokens: Option<u64>,
	pub cost_usd: Option<f64>,
	pub latency_ms: Option<u64>,
	pub request_id: Option<String>,
}

impl Usage {
	/// `chat/completions` の `usage` オブジェクト相当から正規化する。
	/// `input_tokens` / `output_tokens` 系 (Responses / Messages) も受け付ける。
	/// トークン数が1つも読めなければ `None`。
	pub fn from_value(v: &serde_json::Value) -> Option<Self> {
		let prompt = v
			.get("prompt_tokens")
			.and_then(as_u64)
			.or_else(|| v.get("input_tokens").and_then(as_u64))?;
		let completion = v
			.get("completion_tokens")
			.and_then(as_u64)
			.or_else(|| v.get("output_tokens").and_then(as_u64))
			.unwrap_or(0);
		let total = v.get("total_tokens").and_then(as_u64).unwrap_or(prompt + completion);
		let cached = v
			.pointer("/prompt_tokens_details/cached_tokens")
			.and_then(as_u64)
			.or_else(|| v.pointer("/input_tokens_details/cached_tokens").and_then(as_u64))
			.or_else(|| v.get("cache_read_input_tokens").and_then(as_u64));
		let reasoning = v
			.pointer("/completion_tokens_details/reasoning_tokens")
			.and_then(as_u64)
			.or_else(|| v.pointer("/output_tokens_details/reasoning_tokens").and_then(as_u64));
		Some(Self {
			prompt_tokens: prompt,
			completion_tokens: completion,
			total_tokens: total,
			cached_tokens: cached,
			reasoning_tokens: reasoning,
			..Default::default()
		})
	}
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
	if let Some(n) = v.as_u64() {
		return Some(n);
	}
	v.as_f64().map(|f| f as u64)
}

// ---------------------------------------------------------------------------
// Quota: アカウント単位の残高・制限枠 (ポーリング由来)
// ---------------------------------------------------------------------------

/// 5時間 / 週間などのローリング枠。単位はクレジット (USD)。
/// `reset_at_ms` は epoch ミリ秒 (429本文の `rateLimit.reset` は秒なので注意)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaWindow {
	pub used: f64,
	pub cap: f64,
	pub remaining: f64,
	/// 消費率 (0..=100)。`cap <= 0` のときは 0。
	#[serde(default)]
	pub used_pct: f64,
	pub reset_at_ms: Option<i64>,
	pub exceeded: bool,
}

impl QuotaWindow {
	pub fn new(used: f64, cap: f64, reset_at_ms: Option<i64>, exceeded: bool) -> Self {
		let used_pct = if cap > 0.0 {
			(used / cap * 100.0).clamp(0.0, 100.0)
		} else {
			0.0
		};
		Self {
			used,
			cap,
			remaining: (cap - used).max(0.0),
			used_pct,
			reset_at_ms,
			exceeded,
		}
	}

	/// 残量率 (0..=100)。
	pub fn remaining_pct(&self) -> f64 {
		100.0 - self.used_pct
	}
}

/// `GET /alpha/billing/credits` (+ subscriptions / usage/summary) の正規化。
///
/// architecture.md の `Quota { plan, ratelimit_remaining, ratelimit_reset, extra }`
/// に対応する: 5時間枠を `ratelimit_*` とみなす場合は `five_hour` を使う。
/// 生値は `extra` に温存する。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Quota {
	pub plan: Option<String>,
	pub status: Option<String>,
	pub credit_balance: Option<f64>,
	pub monthly_remaining: Option<f64>,
	pub purchased_credits: Option<f64>,
	pub free_credits: Option<f64>,
	pub five_hour: Option<QuotaWindow>,
	pub weekly: Option<QuotaWindow>,
	/// 月次 (請求期間) 枠。APIはcapを返さないため、プロバイダ側で
	/// プラン定義から補完する。`remaining` = `monthly_remaining`、
	/// `reset_at_ms` = 請求期間の終端。
	pub monthly: Option<QuotaWindow>,
	pub period_spend: Option<f64>,
	pub period_tokens: Option<u64>,
	pub extra: serde_json::Value,
}

impl Quota {
	/// architecture.md 互換: 5時間枠の残量をレート制限残量とみなす。
	pub fn ratelimit_remaining(&self) -> Option<f64> {
		self.five_hour.as_ref().map(|w| w.remaining)
	}
	/// architecture.md 互換: 5時間枠のリセット時刻 (epoch ms)。
	pub fn ratelimit_reset(&self) -> Option<i64> {
		self.five_hour.as_ref().and_then(|w| w.reset_at_ms)
	}
}

/// テキスト + 使用量のセット。既存の `String` 返しと両立させるための器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatOutput {
	pub text: String,
	pub usage: Option<Usage>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// 差し替えポイント。すべてのLLM実装はこれを満たす.
///
/// `async_trait` を使うことで `Arc<dyn Provider>` として
/// 動的ディスパッチできる (mock / 本番の差し替え、テスト容易性)。
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
	/// Returns the name of this provider (e.g. "openai", "anthropic", "mock").
	fn name(&self) -> &'static str;

	/// Returns the supported models for this provider. (e.g. "deepseek/deepseek-v4.1-flash", "claude-opus-5-5").
	async fn list_models(&self) -> Result<Vec<ModelInfo>>;

	/// Returns the recommended wire for a given model. (e.g. "chat/completions", "responses", "messages").
	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, max_tokens: Option<u32>) -> Result<String>;

	/// POST /chat/completions 相当 (ストリーム)。
	/// `on_delta` に部分テキストを逐次渡し、戻り値で全文を返す。
	/// `&mut dyn FnMut` にすることで `dyn LlmClient` でも呼べる。
	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String>;

	/// POST /responses 相当 (非ストリーム)。
	async fn responses(&self, model: &str, input: &str) -> Result<String>;

	/// POST /messages 相当 (Anthropic Messages, 非ストリーム)。
	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<String>;

	/// アカウントの残高・制限枠。未対応プロバイダは `Err(Unsupported)`。
	///
	/// Command Code では `GET /alpha/billing/credits` (+ subscriptions /
	/// usage/summary) を正規化する。`/provider/v1/*` に課金面は無い (404)。
	async fn quota(&self) -> Result<Quota> {
		Err(Error::Unsupported("quota".into()))
	}

	/// `chat` + `Usage`。デフォルトは `chat()` のラップ (usageなし)。
	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.chat(model, messages, max_tokens).await?,
			usage: None,
		})
	}

	/// `chat_stream` + `Usage`。デフォルトは `chat_stream()` のラップ。
	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.chat_stream(model, messages, on_delta).await?,
			usage: None,
		})
	}

	/// `responses` + `Usage`。デフォルトは `responses()` のラップ。
	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.responses(model, input).await?,
			usage: None,
		})
	}

	/// `messages` + `Usage`。デフォルトは `messages()` のラップ。
	async fn messages_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.messages(model, messages, system, max_tokens).await?,
			usage: None,
		})
	}
}

/// モデルIDから推奨wireを返す (カタログ無し時のフォールバック)。
pub fn route_for_model(model_id: &str, supported: Option<&[String]>) -> &'static str {
	if model_id.starts_with("claude-") {
		return "messages";
	}
	if let Some(eps) = supported
		&& eps.iter().any(|e| e.contains("/responses"))
	{
		return "responses";
	}
	"chat/completions"
}
