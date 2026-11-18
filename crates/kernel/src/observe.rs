//! Observer 拡張点: ループの出来事を見るだけの口 (ログ・テレメトリなど)。
//!
//! [`Hook`](crate::Hook) と違って書き換えも停止もできず、ループは Observer を待たない。
//! 届くのは Hook をすべて通した後の値 (= 実際にモデル・ツールとやりとりした値)。

use serde::{Deserialize, Serialize};

use crate::{Completion, ModelRequest, ToolCall, ToolOutput};

/// ループの出来事。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
	/// モデルに送るリクエスト。
	Request { request: ModelRequest },
	/// モデルの応答。
	Response { completion: Completion },
	/// モデルに返すツールの結果。`call` は実際に扱った呼び出し (`Skip` されたものも含む)。
	ToolResult { call: ToolCall, output: ToolOutput },
}

/// 出来事を受け取るだけの口。
#[async_trait::async_trait]
pub trait Observer: Send + Sync {
	/// 出来事が起きた順に呼ばれる。ループはこれを待たないので、重い処理はしない。
	fn on_event(&self, event: &Event);

	/// それまでに渡した `on_event` を処理し終えるまで待つ。
	/// [`Session`](crate::Session) の呼び出しが返る前に呼ばれる。
	async fn flush(&self) {}
}
