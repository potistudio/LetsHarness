//! `list_dir`: ディレクトリの直下を一覧する。

use std::sync::Arc;

use letsharness_kernel::{Tool, ToolOutput, ToolSpec};
use serde::Deserialize;

use crate::common::{parse_args, spec};
use crate::workspace::Workspace;

const MAX_LIST_ENTRIES: usize = 1000;

pub(crate) struct ListDir(pub(crate) Arc<Workspace>);

#[derive(Deserialize)]
struct ListArgs {
	path: Option<String>,
}

#[async_trait::async_trait]
impl Tool for ListDir {
	fn spec(&self) -> ToolSpec {
		spec(
			"list_dir",
			"ディレクトリの直下を一覧する。ディレクトリ名は末尾に `/` が付く。",
			serde_json::json!({
				"path": {"type": "string", "description": "作業ディレクトリからの相対パス (既定 `.`)"},
			}),
			&[],
			false,
		)
	}

	async fn call(&self, args: serde_json::Value) -> ToolOutput {
		let a: ListArgs = match parse_args(args) {
			Ok(a) => a,
			Err(e) => return e,
		};
		let raw = a.path.as_deref().unwrap_or(".");
		let path = match self.0.resolve(raw).await {
			Ok(p) => p,
			Err(e) => return ToolOutput::error(e),
		};
		let mut rd = match tokio::fs::read_dir(&path).await {
			Ok(r) => r,
			Err(e) => return ToolOutput::error(format!("{raw}: {e}")),
		};
		let mut names = Vec::new();
		loop {
			match rd.next_entry().await {
				Ok(Some(entry)) => {
					let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
					let mut name = entry.file_name().to_string_lossy().into_owned();
					if is_dir {
						name.push('/');
					}
					names.push(name);
				}
				Ok(None) => break,
				Err(e) => return ToolOutput::error(format!("{raw}: {e}")),
			}
		}
		names.sort();
		let total = names.len();
		let mut out = names.into_iter().take(MAX_LIST_ENTRIES).collect::<Vec<_>>().join("\n");
		if total > MAX_LIST_ENTRIES {
			out.push_str(&format!("\n[{MAX_LIST_ENTRIES} 件まで表示: 全 {total} 件]"));
		}
		if out.is_empty() {
			out = "(空のディレクトリ)".into();
		}
		ToolOutput::ok(out)
	}
}
