//! プラグインの導入: `plugin.toml` と実行ファイルを `<home>/plugins/<name>/` に置き、どこからでも発見されるようにする。
//!
//! 導入元は、プラグイン1つのディレクトリ (`plugin.toml` がある) か、プラグインのディレクトリが並んだディレクトリ
//! (`crates/` など。直下の `*/plugin.toml` をすべて)。実行ファイルは manifest の `command` を起動時と同じ順
//! (manifest のディレクトリ → kernel の実行ファイルと同じ場所) に探して写す。`PATH` にしか無いものは写さず、
//! 起動時も `PATH` から探す。ビルドはしない (先に `cargo build --release` などで作っておく)。

use std::path::{Path, PathBuf};

use crate::external::{MANIFEST_FILE, Manifest, discover};
use crate::profile::home_dir;
use crate::{Error, Result};

/// 導入先 (`<home>/plugins`)。既定の探索先にも入っている。
pub fn plugins_dir() -> Option<PathBuf> {
	home_dir().map(|h| h.join("plugins"))
}

/// 導入した1つのプラグイン。
#[derive(Debug, Clone)]
pub struct Installed {
	pub name: String,
	/// 導入先のディレクトリ。
	pub dir: PathBuf,
	/// 写した実行ファイル。`PATH` から探すものは `None`。
	pub command: Option<PathBuf>,
}

fn io_err(path: &Path, e: std::io::Error) -> Error {
	Error::Config(format!("{}: {e}", path.display()))
}

/// `src` のプラグインを `dest` (ふつうは [`plugins_dir`]) に導入する。
///
/// すでに同じ名前があれば、`force` でなければ何も書かずに `Err`。ひとつでも実行ファイルが見つからなければ、
/// やはり何も書かずに `Err` (中途半端に入れない)。
pub fn install(src: &Path, dest: &Path, force: bool) -> Result<Vec<Installed>> {
	let manifests = if src.join(MANIFEST_FILE).is_file() {
		vec![Manifest::load(&src.join(MANIFEST_FILE))?]
	} else {
		discover(&[src.to_path_buf()])
	};
	if manifests.is_empty() {
		return Err(Error::Config(format!(
			"{}: {MANIFEST_FILE} がありません (直下のディレクトリにも)",
			src.display()
		)));
	}

	// 先に全部確かめてから書く。
	let mut plan = Vec::new();
	for m in &manifests {
		let dir = dest.join(&m.name);
		if dir.exists() && !force {
			return Err(Error::Config(format!(
				"{} はもう入っています ({})。上書きするなら --force",
				m.name,
				dir.display()
			)));
		}
		let command = match m.local_command() {
			Some(path) => Some(path),
			None if m.on_path() => None,
			None => {
				return Err(Error::Config(format!(
					"{}: 実行ファイル `{}` が見つかりません (先にビルドしてください)",
					m.name, m.command
				)));
			}
		};
		plan.push((m, dir, command));
	}

	let mut installed = Vec::new();
	for (m, dir, command) in plan {
		if dir.exists() {
			std::fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
		}
		std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
		let src_manifest = m.dir.join(MANIFEST_FILE);
		let text = std::fs::read_to_string(&src_manifest).map_err(|e| io_err(&src_manifest, e))?;
		let mut table: toml::Table =
			toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", src_manifest.display())))?;
		let copied = match &command {
			Some(path) => {
				let file = path.file_name().expect("a file").to_owned();
				let to = dir.join(&file);
				std::fs::copy(path, &to).map_err(|e| io_err(path, e))?;
				table.insert("command".into(), file.to_string_lossy().into_owned().into());
				Some(to)
			}
			None => None,
		};
		let manifest = dir.join(MANIFEST_FILE);
		let text = toml::to_string_pretty(&table).map_err(|e| Error::Config(format!("{}: {e}", manifest.display())))?;
		std::fs::write(&manifest, text).map_err(|e| io_err(&manifest, e))?;
		installed.push(Installed {
			name: m.name.clone(),
			dir,
			command: copied,
		});
	}
	Ok(installed)
}

/// `dest` から `name` のプラグインを取り除く。`dest` の中の、manifest の名前が一致するディレクトリだけを消す。
pub fn uninstall(name: &str, dest: &Path) -> Result<PathBuf> {
	let found = discover(&[dest.to_path_buf()])
		.into_iter()
		.find(|m| m.name == name)
		.ok_or_else(|| Error::Config(format!("{name} は {} に入っていません", dest.display())))?;
	std::fs::remove_dir_all(&found.dir).map_err(|e| io_err(&found.dir, e))?;
	Ok(found.dir)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn scratch(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("lh-install-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	fn plugin(root: &Path, name: &str, command: &str, with_bin: bool) {
		let dir = root.join(name);
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(
			dir.join(MANIFEST_FILE),
			format!("# comment\nname = \"{name}\"\ncommand = \"{command}\"\n[contributes]\ntool = [\"t\"]\n"),
		)
		.unwrap();
		if with_bin {
			std::fs::write(dir.join(command), "#!/bin/sh\n").unwrap();
		}
	}

	#[test]
	fn installs_every_plugin_and_refuses_to_overwrite() {
		let (src, dest) = (scratch("src"), scratch("dest"));
		plugin(&src, "a", "bin-a", true);
		plugin(&src, "b", "sh", false); // PATH にある
		let got = install(&src, &dest, false).unwrap();
		assert_eq!(got.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
		assert!(dest.join("a/bin-a").is_file());
		assert_eq!(got[1].command, None, "PATH の実行ファイルは写さない");
		let m = Manifest::load(&dest.join("a").join(MANIFEST_FILE)).unwrap();
		assert_eq!((m.command.as_str(), m.contributes.tool.len()), ("bin-a", 1));

		assert!(install(&src.join("a"), &dest, false).is_err());
		assert_eq!(install(&src.join("a"), &dest, true).unwrap().len(), 1);

		assert_eq!(uninstall("a", &dest).unwrap(), dest.join("a"));
		assert!(!dest.join("a").exists());
		assert!(uninstall("a", &dest).is_err());
		std::fs::remove_dir_all(&src).unwrap();
		std::fs::remove_dir_all(&dest).unwrap();
	}

	#[test]
	fn nothing_is_written_when_a_binary_is_missing() {
		let (src, dest) = (scratch("missing"), scratch("missing-dest"));
		plugin(&src, "a", "bin-a", true);
		plugin(&src, "z", "no-such-binary-for-letsharness", false);
		let err = install(&src, &dest, false).unwrap_err();
		assert!(err.to_string().contains("先にビルド"), "{err}");
		assert!(!dest.exists());
		assert!(install(&dest, &src, false).is_err(), "no plugin.toml");
		std::fs::remove_dir_all(&src).unwrap();
	}
}
