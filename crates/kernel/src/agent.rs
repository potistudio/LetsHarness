//! エージェント実行の拡張点。
//!
//! [`AgentBackend`] は「依頼を受けて最終回答まで自走する」もの。`Provider` (1回の呼び出しがモデル1回) とは、
//! エージェントループを誰が回すかで分かれる。
//! - ループ型 (`agent-loop`) : こちらがループを回し、モデルとツールは [`HostApi`] で kernel から借りる。
//! - 外部エージェント (Antigravity CLI など) : ループもツールも相手が持つ。
//!
//! kernel は UI を知らないため、表示・承認は [`AgentHandler`] 経由で Client に委ねる。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ChatMessage, Error, ModelInfo, Provider, Result, ToolCall, ToolOutput, ToolRegistry, ToolSpec, Usage};

/// 1回の `run_agent` でモデルを呼ぶ回数の既定上限 (暴走防止)。
pub const DEFAULT_MAX_STEPS: usize = 20;

/// 実行中のエージェントが kernel に頼むための窓口。モデルやツールを自分では持たないエージェントが使う。
///
/// プラグインは他のプラグインを知らない。kernel が `kind` で引いて貸すだけ。
#[async_trait::async_trait]
pub trait HostApi: Send + Sync {
	/// `kind` の Provider を借りる。
	async fn provider(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn Provider>>;
	/// ツールを供給するプラグインのツールをすべて借りる。
	async fn tools(&self, cfg: &Value) -> Result<ToolRegistry>;
}

/// ループの進行を観測し、承認を返す口。Client が実装する。
///
/// 通知系はすべて既定で何もしない。`approve` の既定は **拒否** (安全側):
/// 承認が要るツールは、Handler が明示的に許可しない限り実行されない。
#[async_trait::async_trait]
pub trait AgentHandler: Send {
	/// ツール呼び出しに添えられた assistant の前置き本文。
	/// 最終回答では呼ばれない (`AgentOutcome::text` で返る)。
	fn on_text(&mut self, _text: &str) {}
	fn on_thinking(&mut self, _thinking: &str) {}
	/// 実行しようとしている呼び出し (承認前に通知)。
	fn on_tool_call(&mut self, _call: &ToolCall) {}
	/// 実行結果 (拒否・未知のツールもここに来る)。
	fn on_tool_result(&mut self, _call: &ToolCall, _output: &ToolOutput) {}
	/// `ToolSpec::requires_approval` なツールの実行可否。
	async fn approve(&mut self, _call: &ToolCall, _spec: &ToolSpec) -> bool {
		false
	}
	/// この実行を呼び出した kernel への窓口。外部プロセスのプラグインとして動いているときだけ `Some`。
	fn host(&self) -> Option<&dyn HostApi> {
		None
	}
}

/// 承認が必要なツール (`ToolSpec::requires_approval`) の扱い。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalPolicy {
	/// 実行ごとに [`AgentHandler::approve`] で確認する。
	///
	/// 確認の口を持たない外部エージェント ([`AgentBackend::asks_approval`] が `false`) では、
	/// エージェント側の設定で事前に許可されていないものは却下される。
	#[default]
	Ask,
	/// 確認なしで全て実行する。
	AutoApprove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
	/// 空ならバックエンドの既定モデル (対応していれば)。
	pub model: String,
	pub messages: Vec<ChatMessage>,
	pub max_tokens: Option<u32>,
	/// モデル呼び出しの上限回数。外部エージェントでは効かないことがある。
	pub max_steps: usize,
	pub approval: ApprovalPolicy,
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
			approval: ApprovalPolicy::default(),
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

	/// ツール実行前に [`AgentHandler::approve`] を呼べるか。
	/// `false` のバックエンドでは承認は [`ApprovalPolicy`] (起動時の権限) でしか制御できず、
	/// ハンドラには実行の通知 (`on_tool_call` / `on_tool_result`) だけが届く。
	fn asks_approval(&self) -> bool {
		true
	}

	/// 使えるモデルの一覧。
	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		Err(Error::Unsupported("list_models".into()))
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn AgentHandler) -> Result<AgentOutcome>;
}
