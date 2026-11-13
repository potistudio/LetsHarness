//! 作業ディレクトリ (`root`) の管理と、root からの逸脱検査。

use std::path::{Component, Path, PathBuf};

pub(crate) struct Workspace {
	/// 正規化済みの root (取れなければ渡された値のまま)。
	pub(crate) root: PathBuf,
}

impl Workspace {
	pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
		let root = root.into();
		Self {
			root: std::fs::canonicalize(&root).unwrap_or(root),
		}
	}

	/// `raw` を root 基準の絶対パスにする。root の外に出るなら `Err(理由)`。
	///
	/// 存在しないパス (これから作るファイル) も扱えるよう、`..` を字面で畳んだ上で
	/// 「存在する最長の祖先」だけ実体 (シンボリックリンク解決) を取って検査する。
	pub(crate) async fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
		if raw.is_empty() {
			return Err("path が空です".into());
		}
		let joined = self.root.join(raw); // raw が絶対パスなら raw になる
		let mut normal = PathBuf::new();
		for c in joined.components() {
			match c {
				Component::ParentDir => {
					normal.pop();
				}
				Component::CurDir => {}
				c => normal.push(c.as_os_str()),
			}
		}

		let mut existing = normal.as_path();
		let mut rest = Vec::new();
		while tokio::fs::symlink_metadata(existing).await.is_err() {
			match (existing.file_name(), existing.parent()) {
				(Some(name), Some(parent)) => {
					rest.push(name.to_os_string());
					existing = parent;
				}
				_ => break,
			}
		}
		let mut real = tokio::fs::canonicalize(existing)
			.await
			.map_err(|e| format!("{raw}: {e}"))?;
		real.extend(rest.iter().rev());

		if real.starts_with(&self.root) {
			Ok(real)
		} else {
			Err(format!(
				"{raw}: 作業ディレクトリ ({}) の外は扱えません",
				self.root.display()
			))
		}
	}

	/// 表示用に root からの相対パスにする。
	pub(crate) fn display(&self, p: &Path) -> String {
		p.strip_prefix(&self.root).unwrap_or(p).display().to_string()
	}
}
