//! `bash`: シェルコマンドを作業ディレクトリで実行する。

use std::sync::Arc;
use std::time::Duration;

use letsharness_kernel::{Tool, ToolOutput, ToolSpec};
use serde::Deserialize;

use crate::common::{parse_args, spec};
use crate::workspace::Workspace;

/// 出力の上限 (超えたら中央を省く)。
const MAX_BASH_OUTPUT: usize = 30_000;
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
const MAX_BASH_TIMEOUT_SECS: u64 = 600;

pub(crate) struct Bash(pub(crate) Arc<Workspace>);

#[derive(Deserialize)]
struct BashArgs {
	command: String,
	timeout_secs: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for Bash {
	fn spec(&self) -> ToolSpec {
		spec(
			"bash",
			"シェルコマンドを作業ディレクトリで実行し、終了コードと stdout/stderr を返す。\
			 stdin は空。タイムアウトすると打ち切る。",
			serde_json::json!({
				"command": {"type": "string", "description": "`sh -c` に渡すコマンド"},
				"timeout_secs": {
					"type": "integer",
					"description": format!("タイムアウト秒 (既定 {DEFAULT_BASH_TIMEOUT_SECS}, 最大 {MAX_BASH_TIMEOUT_SECS})"),
				},
			}),
			&["command"],
		)
	}

	async fn call(&self, args: serde_json::Value) -> ToolOutput {
		let a: BashArgs = match parse_args(args) {
			Ok(a) => a,
			Err(e) => return e,
		};
		let secs = a
			.timeout_secs
			.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS)
			.clamp(1, MAX_BASH_TIMEOUT_SECS);
		let mut cmd = tokio::process::Command::new("sh");
		cmd.arg("-c")
			.arg(&a.command)
			.current_dir(&self.0.root)
			.stdin(std::process::Stdio::null())
			// タイムアウトで future が落ちたら子を kill する。
			.kill_on_drop(true);

		let output = match tokio::time::timeout(Duration::from_secs(secs), cmd.output()).await {
			Ok(Ok(o)) => o,
			Ok(Err(e)) => return ToolOutput::error(format!("起動に失敗: {e}")),
			Err(_) => return ToolOutput::error(format!("{secs} 秒でタイムアウトしました")),
		};

		let mut text = String::new();
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);
		if !stdout.is_empty() {
			text.push_str(&stdout);
		}
		if !stderr.is_empty() {
			if !text.is_empty() && !text.ends_with('\n') {
				text.push('\n');
			}
			text.push_str("[stderr]\n");
			text.push_str(&stderr);
		}
		let text = truncate_middle(&text, MAX_BASH_OUTPUT);
		match output.status.code() {
			Some(0) => ToolOutput::ok(if text.is_empty() { "(出力なし)".into() } else { text }),
			Some(code) => ToolOutput::error(format!("exit code {code}\n{text}")),
			None => ToolOutput::error(format!("シグナルで終了\n{text}")),
		}
	}
}

/// `max` バイトを超えたら先頭と末尾を残して中央を省く (文字境界は守る)。
fn truncate_middle(s: &str, max: usize) -> String {
	if s.len() <= max {
		return s.to_string();
	}
	let mut head = max / 2;
	while !s.is_char_boundary(head) {
		head -= 1;
	}
	let mut tail = s.len() - max / 2;
	while !s.is_char_boundary(tail) {
		tail += 1;
	}
	format!(
		"{}\n... [{} bytes omitted] ...\n{}",
		&s[..head],
		tail - head,
		&s[tail..]
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn truncate_middle_keeps_ends_and_char_boundaries() {
		assert_eq!(truncate_middle("short", 100), "short");
		let s = "あ".repeat(100);
		let t = truncate_middle(&s, 50);
		assert!(t.starts_with('あ') && t.ends_with('あ') && t.contains("bytes omitted"));
	}
}
