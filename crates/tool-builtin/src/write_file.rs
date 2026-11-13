//! `write_file`: ファイルを丸ごと書く。

use std::sync::Arc;

use letsharness_kernel::{Tool, ToolOutput, ToolSpec};
use serde::Deserialize;

use crate::common::{parse_args, spec};
use crate::workspace::Workspace;

pub(crate) struct WriteFile(pub(crate) Arc<Workspace>);

#[derive(Deserialize)]
struct WriteArgs {
	path: String,
	content: String,
}

#[async_trait::async_trait]
impl Tool for WriteFile {
	fn spec(&self) -> ToolSpec {
		spec(
			"write_file",
			"ファイルを丸ごと書く (既存なら上書き, 親ディレクトリは作る)。部分的な変更は edit_file を使う。",
			serde_json::json!({
				"path": {"type": "string", "description": "作業ディレクトリからの相対パス"},
				"content": {"type": "string", "description": "書き込む全文"},
			}),
			&["path", "content"],
			true,
		)
	}

	async fn call(&self, args: serde_json::Value) -> ToolOutput {
		let a: WriteArgs = match parse_args(args) {
			Ok(a) => a,
			Err(e) => return e,
		};
		let path = match self.0.resolve(&a.path).await {
			Ok(p) => p,
			Err(e) => return ToolOutput::error(e),
		};
		if let Some(parent) = path.parent()
			&& let Err(e) = tokio::fs::create_dir_all(parent).await
		{
			return ToolOutput::error(format!("{}: {e}", a.path));
		}
		match tokio::fs::write(&path, a.content.as_bytes()).await {
			Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {}", a.content.len(), self.0.display(&path))),
			Err(e) => ToolOutput::error(format!("{}: {e}", a.path)),
		}
	}
}
