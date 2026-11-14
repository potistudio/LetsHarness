//! `edit_file`: 既存ファイル内の文字列を置換する。

use std::sync::Arc;

use letsharness_kernel::{Tool, ToolOutput, ToolSpec};
use serde::Deserialize;

use crate::common::{parse_args, spec};
use crate::workspace::Workspace;

pub(crate) struct EditFile(pub(crate) Arc<Workspace>);

#[derive(Deserialize)]
struct EditArgs {
	path: String,
	old_string: String,
	new_string: String,
	#[serde(default)]
	replace_all: bool,
}

#[async_trait::async_trait]
impl Tool for EditFile {
	fn spec(&self) -> ToolSpec {
		spec(
			"edit_file",
			"既存ファイル内の文字列を置換する。old_string はファイル内でちょうど1か所に一致する長さの文脈を含めること \
			 (複数一致はエラー)。全置換は replace_all=true。",
			serde_json::json!({
				"path": {"type": "string", "description": "作業ディレクトリからの相対パス"},
				"old_string": {"type": "string", "description": "置換前の文字列 (空不可, 空白・改行も厳密一致)"},
				"new_string": {"type": "string", "description": "置換後の文字列"},
				"replace_all": {"type": "boolean", "description": "全一致を置換する (既定 false)"},
			}),
			&["path", "old_string", "new_string"],
		)
	}

	async fn call(&self, args: serde_json::Value) -> ToolOutput {
		let a: EditArgs = match parse_args(args) {
			Ok(a) => a,
			Err(e) => return e,
		};
		if a.old_string.is_empty() {
			return ToolOutput::error("old_string が空です");
		}
		if a.old_string == a.new_string {
			return ToolOutput::error("old_string と new_string が同じです");
		}
		let path = match self.0.resolve(&a.path).await {
			Ok(p) => p,
			Err(e) => return ToolOutput::error(e),
		};
		let text = match tokio::fs::read_to_string(&path).await {
			Ok(t) => t,
			Err(e) => return ToolOutput::error(format!("{}: {e}", a.path)),
		};
		let count = text.matches(a.old_string.as_str()).count();
		match (count, a.replace_all) {
			(0, _) => return ToolOutput::error(format!("{}: old_string が見つかりません", a.path)),
			(n, false) if n > 1 => {
				return ToolOutput::error(format!(
					"{}: old_string が {n} か所に一致します。文脈を足して一意にするか replace_all=true にする",
					a.path
				));
			}
			_ => {}
		}
		let updated = text.replace(a.old_string.as_str(), &a.new_string);
		match tokio::fs::write(&path, updated).await {
			Ok(()) => ToolOutput::ok(format!("edited {} ({count} 件置換)", self.0.display(&path))),
			Err(e) => ToolOutput::error(format!("{}: {e}", a.path)),
		}
	}
}
