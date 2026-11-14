//! 実行の途中経過を受け取る口。ループ (`run_loop`) と外部エージェント (`AgentBackend`) で共通。
//!
//! kernel は UI を知らないため、表示は [`RunHandler`] 経由で Client に委ねる。

use crate::{ToolCall, ToolOutput};

/// モデル呼び出しの回数の既定上限 (暴走防止)。
pub const DEFAULT_MAX_STEPS: usize = 20;

/// 実行の進行を観測する口。Client が実装する。
///
/// 通知はすべて既定で何もしない。
pub trait RunHandler: Send {
	/// ツール呼び出しに添えられた assistant の前置き本文。最終回答では呼ばれない (結果の `text` で返る)。
	fn on_text(&mut self, _text: &str) {}
	fn on_thinking(&mut self, _thinking: &str) {}
	/// 実行しようとしている呼び出し。
	fn on_tool_call(&mut self, _call: &ToolCall) {}
	/// 実行結果 (未知のツールもここに来る)。
	fn on_tool_result(&mut self, _call: &ToolCall, _output: &ToolOutput) {}
}
