//! 外部エージェントの拡張点。
//!
//! [`AgentBackend`] は「依頼を受けて、ループもツールも自分で持って最終回答まで自走する」もの
//! (Antigravity CLI など)。こちらがモデルとツールでループを回す場合は [`run_loop`](crate::run_loop) で、
//! Agent とは呼ばない。`Provider` (1回の呼び出しがモデル1回) とも別。
//!
//! 表示は [`RunHandler`] 経由で Client に委ねる。

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, DEFAULT_MAX_STEPS, Error, ModelInfo, Result, RunHandler, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
	/// 空ならバックエンドの既定モデル (対応していれば)。
	pub model: String,
	pub messages: Vec<ChatMessage>,
	pub max_tokens: Option<u32>,
	/// モデル呼び出しの上限回数。外部エージェントでは効かないことがある。
	pub max_steps: usize,
	/// 外部エージェントが持つ会話を再開する ID ([`AgentOutcome::session_id`])。
	/// 指定時、相手は履歴を持っているので `messages` は最後の依頼だけが使われる。
	pub session_id: Option<String>,
}

impl AgentRequest {
	pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
		Self {
			model: model.into(),
			messages,
			max_tokens: None,
			max_steps: DEFAULT_MAX_STEPS,
			session_id: None,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutcome {
	/// 最終ターンの本文。
	pub text: String,
	/// 入力 + 途中のツール往復 + 最終回答を含む全履歴 (会話の継続に使える)。
	pub messages: Vec<ChatMessage>,
	/// モデルを呼んだ回数 (外部エージェントでは観測できた範囲の推定値)。
	pub steps: usize,
	/// 全ステップ合算の使用量 (どのステップも usage を返さなければ `None`)。
	pub usage: Option<Usage>,
	/// `max_steps` に達して打ち切った (最終ターンがまだツールを要求していた)。
	/// このとき `messages` は未実行のツール要求を含まない。
	pub hit_step_limit: bool,
	/// 外部エージェントの会話 ID。[`AgentRequest::session_id`] に渡すと続きから再開できる。
	/// 組み込みループは `messages` で継続するため `None`。
	pub session_id: Option<String>,
}

/// エージェント実行の拡張点。
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
	/// 例: `"loop"` / `"antigravity"`。
	fn name(&self) -> &'static str;

	/// 使えるモデルの一覧。
	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		Err(Error::Unsupported("list_models".into()))
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn RunHandler) -> Result<AgentOutcome>;
}
