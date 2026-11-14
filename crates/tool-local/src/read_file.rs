//! `read_file`: テキストファイルを行番号付きで読む。

use std::sync::Arc;

use letsharness_kernel::{Tool, ToolOutput, ToolSpec};
use serde::Deserialize;

use crate::common::{parse_args, spec};
use crate::workspace::Workspace;

/// 一度に返す最大バイト数。
const MAX_READ_BYTES: usize = 100_000;
/// 開く最大ファイルサイズ。
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_READ_LINES: usize = 2000;

pub(crate) struct ReadFile(pub(crate) Arc<Workspace>);

#[derive(Deserialize)]
struct ReadArgs {
	path: String,
	/// 開始行 (1始まり)。
	offset: Option<usize>,
	limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
	fn spec(&self) -> ToolSpec {
		spec(
			"read_file",
			"テキストファイルを行番号付きで読む。大きいファイルは offset (1始まりの開始行) と limit (行数) で区切る。",
			serde_json::json!({
				"path": {"type": "string", "description": "作業ディレクトリからの相対パス"},
				"offset": {"type": "integer", "description": "開始行 (1始まり, 既定 1)"},
				"limit": {"type": "integer", "description": "読む行数 (既定 2000)"},
			}),
			&["path"],
			false,
		)
	}

	async fn call(&self, args: serde_json::Value) -> ToolOutput {
		let a: ReadArgs = match parse_args(args) {
			Ok(a) => a,
			Err(e) => return e,
		};
		let path = match self.0.resolve(&a.path).await {
			Ok(p) => p,
			Err(e) => return ToolOutput::error(e),
		};
		match tokio::fs::metadata(&path).await {
			Ok(m) if m.is_dir() => return ToolOutput::error(format!("{}: ディレクトリです (list_dir を使う)", a.path)),
			Ok(m) if m.len() > MAX_FILE_BYTES => {
				return ToolOutput::error(format!("{}: {} bytes は大きすぎて読めません", a.path, m.len()));
			}
			Ok(_) => {}
			Err(e) => return ToolOutput::error(format!("{}: {e}", a.path)),
		}
		let bytes = match tokio::fs::read(&path).await {
			Ok(b) => b,
			Err(e) => return ToolOutput::error(format!("{}: {e}", a.path)),
		};
		if bytes.contains(&0) {
			return ToolOutput::error(format!("{}: バイナリファイルは読めません", a.path));
		}
		let text = String::from_utf8_lossy(&bytes);

		let start = a.offset.unwrap_or(1).max(1);
		let limit = a.limit.unwrap_or(DEFAULT_READ_LINES);
		let total = text.lines().count();
		let mut out = String::new();
		let mut shown = 0;
		for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
			let numbered = format!("{}\t{line}\n", i + 1);
			if out.len() + numbered.len() > MAX_READ_BYTES {
				break;
			}
			out.push_str(&numbered);
			shown += 1;
		}
		if shown == 0 && total >= start {
			return ToolOutput::error(format!("{}: 1行が大きすぎて返せません", a.path));
		}
		let end = start + shown;
		if end <= total {
			out.push_str(&format!(
				"[{} 行目以降は省略: 全 {total} 行。offset={end} で続きを読める]\n",
				end
			));
		}
		if out.is_empty() {
			out = format!("(空: 全 {total} 行, offset={start})\n");
		}
		ToolOutput::ok(out)
	}
}
