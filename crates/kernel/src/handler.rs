//! 実行の途中経過を受け取り、承認を返す口。ループ (`run_loop`) と外部エージェント (`AgentBackend`) で共通。
//!
//! kernel は UI を知らないため、表示・承認は [`RunHandler`] 経由で Client に委ねる。

use serde::{Deserialize, Serialize};

use crate::{ToolCall, ToolOutput, ToolSpec};

/// モデル呼び出しの回数の既定上限 (暴走防止)。
pub const DEFAULT_MAX_STEPS: usize = 20;

/// 実行の進行を観測し、承認を返す口。Client が実装する。
///
/// 通知系はすべて既定で何もしない。`approve` の既定は **拒否** (安全側):
/// 承認が要るツールは、Handler が明示的に許可しない限り実行されない。
#[async_trait::async_trait]
pub trait RunHandler: Send {
	/// ツール呼び出しに添えられた assistant の前置き本文。最終回答では呼ばれない (結果の `text` で返る)。
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
}

/// 承認が必要なツール (`ToolSpec::requires_approval`) の扱い。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalPolicy {
	/// 実行ごとに [`RunHandler::approve`] で確認する。
	///
	/// 確認の口を持たない外部エージェント ([`AgentBackend::asks_approval`](crate::AgentBackend::asks_approval) が `false`) では、
	/// エージェント側の設定で事前に許可されていないものは却下される。
	#[default]
	Ask,
	/// 確認なしで全て実行する。
	AutoApprove,
}
