//! ローカルツール: `read_file` / `list_dir` / `write_file` / `edit_file` / `bash`.
//!
//! すべて作業ディレクトリ (`root`) を基準に動く。
//!
//! - ファイル系ツールは `root` の外を指すパス (`..` 経由・絶対パス・シンボリックリンク) を拒否する。
//! - **`bash` はサンドボックスではない。** `root` の制限は cwd を決めるだけで、
//!   実行されるコマンドは何でもできる。実行を止める仕組みは無い。

mod plugin;

pub use plugin::server;

mod bash;
mod common;
mod edit_file;
mod list_dir;
mod read_file;
mod workspace;
mod write_file;

use std::path::PathBuf;
use std::sync::Arc;

use letsharness_kernel::Tool;

use bash::Bash;
use edit_file::EditFile;
use list_dir::ListDir;
use read_file::ReadFile;
use workspace::Workspace;
use write_file::WriteFile;

/// 同梱ツール一式。`root` は作業ディレクトリ。
pub fn local_tools(root: impl Into<PathBuf>) -> Vec<Arc<dyn Tool>> {
	let ws = Arc::new(Workspace::new(root));
	vec![
		Arc::new(ReadFile(ws.clone())),
		Arc::new(ListDir(ws.clone())),
		Arc::new(WriteFile(ws.clone())),
		Arc::new(EditFile(ws.clone())),
		Arc::new(Bash(ws)),
	]
}
