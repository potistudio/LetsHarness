//! LetsHarness kernel: 拡張点の trait と共有型.
//!
//! bin やハーネス本体はこの crate だけに依存し、
//! 具体的な実装 (`commandcode` / `mock` / 将来の他社API) を
//! `Arc<dyn Provider>` として差し替えられる。
//!
//! この crate は UI・HTTP ライブラリ (`clap` / `reqwest` など) に依存してはならない。

use serde::{Deserialize, Serialize};

mod agent;
mod dispatch;
mod external;
mod handler;
mod host;
mod kernel;
mod rpc;
mod run_loop;
mod serve;
mod tool;

pub use agent::{AgentBackend, AgentOutcome, AgentRequest};
pub use external::{Contributes, MANIFEST_FILE, Manifest};
pub use handler::{DEFAULT_MAX_STEPS, RunHandler};
pub use host::Host;
pub use kernel::{BootConfig, Kernel};
pub use run_loop::{LoopOutcome, LoopRequest, run_loop};
pub use serve::{AgentBackendFactory, BootHook, PluginServer, ProviderFactory, ToolFactory};
pub use tool::{Tool, ToolCall, ToolOutput, ToolRegistry, ToolSpec};

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

/// 1メッセージ。`role` は `system` / `user` / `assistant` / `tool`。
///
/// ツール実行用の2フィールドは通常のチャットでは空で、シリアライズにも出ない:
/// - `assistant` : モデルが要求したツール呼び出し (`tool_calls`)
/// - `tool`      : 実行結果 (`tool_call_id` で呼び出しと対応づける)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
	pub role: String,
	pub content: String,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub tool_calls: Vec<ToolCall>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool_call_id: Option<String>,
}

impl ChatMessage {
	fn plain(role: &str, content: impl Into<String>) -> Self {
		Self {
			role: role.into(),
			content: content.into(),
			tool_calls: Vec::new(),
			tool_call_id: None,
		}
	}
	pub fn user(content: impl Into<String>) -> Self {
		Self::plain("user", content)
	}
	pub fn system(content: impl Into<String>) -> Self {
		Self::plain("system", content)
	}
	pub fn assistant(content: impl Into<String>) -> Self {
		Self::plain("assistant", content)
	}
	/// ツール呼び出しを伴う assistant メッセージ。`content` は前置きの本文 (空でよい)。
	pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
		Self {
			tool_calls,
			..Self::plain("assistant", content)
		}
	}
	/// ツール実行結果。`tool_call_id` は対応する [`ToolCall::id`]。
	pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
		Self {
			tool_call_id: Some(tool_call_id.into()),
			..Self::plain("tool", content)
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
///   (+ `prompt_tokens_details.cached_tokens`)
/// - Responses: `input_tokens` / `output_tokens`
///   (+ `input_tokens_details.cached_tokens`)
/// - Messages (Anthropic): `input_tokens` / `output_tokens` +
///   `cache_read_input_tokens` / `cache_creation_input_tokens`
///
/// 正規化後の `prompt_tokens` は **キャッシュ分を含む入力総数**。
/// OpenAI系は元から内数だが、Anthropic の `input_tokens` は
/// キャッシュ読み書きを含まない外数なので加算して揃える。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
	/// 入力総数 (キャッシュ読み書き込み)。
	pub prompt_tokens: u64,
	pub completion_tokens: u64,
	pub total_tokens: u64,
	/// キャッシュから読んだ入力トークン (`prompt_tokens` の内数)。
	pub cached_tokens: Option<u64>,
	/// キャッシュに書いた入力トークン (`prompt_tokens` の内数, Anthropicのみ)。
	#[serde(default)]
	pub cache_write_tokens: Option<u64>,
	pub reasoning_tokens: Option<u64>,
	pub cost_usd: Option<f64>,
	/// リクエスト送信から応答完了までの壁時計時間。
	pub latency_ms: Option<u64>,
	/// 最初の出力トークンが届くまでの時間 (ストリーム時のみ)。
	#[serde(default)]
	pub ttft_ms: Option<u64>,
	pub request_id: Option<String>,
}

impl Usage {
	/// `chat/completions` の `usage` オブジェクト相当から正規化する。
	/// `input_tokens` / `output_tokens` 系 (Responses / Messages) も受け付ける。
	/// トークン数が1つも読めなければ `None`。
	pub fn from_value(v: &serde_json::Value) -> Option<Self> {
		let cache_read = v.get("cache_read_input_tokens").and_then(as_u64);
		let cache_write = v.get("cache_creation_input_tokens").and_then(as_u64);
		let prompt = match v.get("prompt_tokens").and_then(as_u64) {
			Some(p) => p,
			None => {
				let input = v.get("input_tokens").and_then(as_u64)?;
				// Anthropic形 (cache_*_input_tokens あり) は外数なので足す。
				input + cache_read.unwrap_or(0) + cache_write.unwrap_or(0)
			}
		};
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
			.or(cache_read);
		let reasoning = v
			.pointer("/completion_tokens_details/reasoning_tokens")
			.and_then(as_u64)
			.or_else(|| v.pointer("/output_tokens_details/reasoning_tokens").and_then(as_u64));
		Some(Self {
			prompt_tokens: prompt,
			completion_tokens: completion,
			total_tokens: total,
			cached_tokens: cached,
			cache_write_tokens: cache_write,
			reasoning_tokens: reasoning,
			..Default::default()
		})
	}

	/// 入力 (in) 総数。キャッシュ分を含む。
	pub fn input_tokens(&self) -> u64 {
		self.prompt_tokens
	}

	/// 出力 (out) 総数。reasoning を含む。
	pub fn output_tokens(&self) -> u64 {
		self.completion_tokens
	}

	/// キャッシュから読んだ入力トークン (無報告は 0)。
	pub fn cached_input_tokens(&self) -> u64 {
		self.cached_tokens.unwrap_or(0).min(self.prompt_tokens)
	}

	/// キャッシュに乗らなかった入力トークン (= in - cached)。
	pub fn uncached_input_tokens(&self) -> u64 {
		self.prompt_tokens - self.cached_input_tokens()
	}

	/// キャッシュヒット率 (0..=100)。入力0 or 未報告なら `None`。
	pub fn cache_hit_pct(&self) -> Option<f64> {
		if self.prompt_tokens == 0 {
			return None;
		}
		self.cached_tokens
			.map(|_| self.cached_input_tokens() as f64 / self.prompt_tokens as f64 * 100.0)
	}

	/// 出力スループット (tok/s)。
	///
	/// `ttft_ms` があれば生成区間 (`latency - ttft`) で割る (= decode速度)。
	/// 無ければ `latency` 全体で割る (prefill・ネットワーク込みの実効速度)。
	/// 時間が取れない / 0 なら `None`。
	pub fn output_tokens_per_sec(&self) -> Option<f64> {
		let latency = self.latency_ms?;
		let gen_ms = match self.ttft_ms {
			Some(t) if latency > t => latency - t,
			_ => latency,
		};
		if gen_ms == 0 {
			return None;
		}
		Some(self.completion_tokens as f64 / (gen_ms as f64 / 1000.0))
	}

	/// 派生値のまとめ (表示・JSON出力用)。
	pub fn stats(&self) -> UsageStats {
		UsageStats {
			input_tokens: self.input_tokens(),
			output_tokens: self.output_tokens(),
			cached_tokens: self.cached_input_tokens(),
			uncached_input_tokens: self.uncached_input_tokens(),
			cache_hit_pct: self.cache_hit_pct(),
			output_tokens_per_sec: self.output_tokens_per_sec(),
		}
	}
}

/// [`Usage`] から計算した派生値。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageStats {
	pub input_tokens: u64,
	pub output_tokens: u64,
	pub cached_tokens: u64,
	pub uncached_input_tokens: u64,
	pub cache_hit_pct: Option<f64>,
	pub output_tokens_per_sec: Option<f64>,
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

/// テキスト + 思考過程 + 使用量のセット。既存の `String` 返しと両立させるための器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatOutput {
	pub text: String,
	/// モデルの思考過程 (reasoning / extended thinking)。無ければ `None`。
	#[serde(default)]
	pub thinking: Option<String>,
	pub usage: Option<Usage>,
}

impl ChatOutput {
	/// 空の思考過程 (`Some("")` や空白のみ) を `None` に正規化する。
	pub fn normalized_thinking(thinking: Option<String>) -> Option<String> {
		thinking.filter(|t| !t.trim().is_empty())
	}
}

/// ツール付きリクエスト1回分の結果。`tool_calls` が空なら最終回答。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Completion {
	pub text: String,
	#[serde(default)]
	pub thinking: Option<String>,
	/// モデルが要求したツール呼び出し (順序保持)。
	#[serde(default)]
	pub tool_calls: Vec<ToolCall>,
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
	/// `on_delta` に部分テキスト、`on_thinking` に思考過程の差分を逐次渡し、戻り値で全文を返す。
	/// `&mut dyn FnMut` にすることで `dyn LlmClient` でも呼べる。
	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
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

	/// `chat` + `Usage` + 思考過程。デフォルトは `chat()` のラップ (usage / thinkingなし)。
	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.chat(model, messages, max_tokens).await?,
			thinking: None,
			usage: None,
		})
	}

	/// `chat_stream` + `Usage` + 思考過程。デフォルトは `chat_stream()` のラップ。
	/// 思考差分は `on_thinking` にそのまま流し、全文は `thinking` にもまとめる。
	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let mut thinking_buf = String::new();
		let text = {
			let thinking_buf_ref = &mut thinking_buf;
			let wrapped_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send) = &mut |chunk: &str| {
				thinking_buf_ref.push_str(chunk);
				on_thinking(chunk);
			};
			self.chat_stream(model, messages, on_delta, wrapped_thinking).await?
		};
		Ok(ChatOutput {
			text,
			thinking: ChatOutput::normalized_thinking(Some(thinking_buf)),
			usage: None,
		})
	}

	/// ツール定義つきの1ターン (非ストリーム)。ツール実行ループは [`run_loop`] が回す。
	///
	/// `tools` が空なら通常の `chat_with_usage` と同じ。空でないのに未対応の
	/// プロバイダは `Err(Unsupported("tools"))` を返す。wire の選択は実装側の事情。
	async fn chat_with_tools(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		if !tools.is_empty() {
			return Err(Error::Unsupported("tools".into()));
		}
		let out = self.chat_with_usage(model, messages, max_tokens).await?;
		Ok(Completion {
			text: out.text,
			thinking: out.thinking,
			tool_calls: Vec::new(),
			usage: out.usage,
		})
	}

	/// `responses` + `Usage`。デフォルトは `responses()` のラップ。
	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		Ok(ChatOutput {
			text: self.responses(model, input).await?,
			thinking: None,
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
			thinking: None,
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn anthropic_cache_is_added_to_input() {
		let u = Usage::from_value(&serde_json::json!({
			"input_tokens": 10, "output_tokens": 5,
			"cache_read_input_tokens": 80, "cache_creation_input_tokens": 10
		}))
		.unwrap();
		assert_eq!(u.prompt_tokens, 100);
		assert_eq!(u.total_tokens, 105);
		assert_eq!(u.cached_tokens, Some(80));
		assert_eq!(u.cache_write_tokens, Some(10));
		assert_eq!(u.uncached_input_tokens(), 20);
		assert_eq!(u.cache_hit_pct(), Some(80.0));
	}

	#[test]
	fn openai_cache_is_already_included() {
		let u = Usage::from_value(&serde_json::json!({
			"prompt_tokens": 200, "completion_tokens": 4,
			"prompt_tokens_details": {"cached_tokens": 150}
		}))
		.unwrap();
		assert_eq!(u.input_tokens(), 200);
		assert_eq!(u.uncached_input_tokens(), 50);
		assert_eq!(u.cache_hit_pct(), Some(75.0));
	}

	#[test]
	fn cache_hit_none_when_unreported() {
		let u = Usage {
			prompt_tokens: 10,
			..Default::default()
		};
		assert_eq!(u.cache_hit_pct(), None);
		assert_eq!(u.uncached_input_tokens(), 10);
		assert_eq!(Usage::default().cache_hit_pct(), None);
	}

	#[test]
	fn tokens_per_sec() {
		let mut u = Usage {
			completion_tokens: 100,
			latency_ms: Some(2000),
			..Default::default()
		};
		assert_eq!(u.output_tokens_per_sec(), Some(50.0));
		// TTFTがあれば生成区間だけで割る
		u.ttft_ms = Some(1000);
		assert_eq!(u.output_tokens_per_sec(), Some(100.0));
		// ttft >= latency は全体で割るにフォールバック
		u.ttft_ms = Some(5000);
		assert_eq!(u.output_tokens_per_sec(), Some(50.0));
		u.latency_ms = Some(0);
		u.ttft_ms = None;
		assert_eq!(u.output_tokens_per_sec(), None);
		u.latency_ms = None;
		assert_eq!(u.output_tokens_per_sec(), None);
	}
}
