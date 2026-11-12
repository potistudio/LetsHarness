//! LLMプロバイダを差し替え可能にするトレイト層.
//!
//! `main.rs` やハーネス本体はこのトレイトだけに依存し、
//! 具体的な実装 (`commandcode` / `mock` / 将来の他社API) を
//! `Arc<dyn LlmClient>` として差し替えられる。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("APIキーが見つかりません。COMMAND_CODE_API_KEY を設定するか `pi` で /login してください")]
	AuthMissing,
	#[error("transport: {0}")]
	Transport(#[from] reqwest::Error),
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
	#[error("APIエラー status={status} code={code:?} message={message}")]
	Api {
		status: u16,
		code: Option<String>,
		message: String,
	},
	#[error("mock: {0}")]
	Mock(String),
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
// Trait
// ---------------------------------------------------------------------------

/// 差し替えポイント。すべてのLLM実装はこれを満たす.
///
/// `async_trait` を使うことで `Arc<dyn LlmClient>` として
/// 動的ディスパッチできる (mock / 本番の差し替え、テスト容易性)。
#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
	/// プロバイダ名 (`commandcode` / `mock` など)。ログ・選択用。
	fn name(&self) -> &'static str;

	/// GET /models 相当。認証不要な実装もあり得る。
	async fn list_models(&self) -> Result<Vec<ModelInfo>>;

	/// POST /chat/completions 相当 (非ストリーム)。
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
