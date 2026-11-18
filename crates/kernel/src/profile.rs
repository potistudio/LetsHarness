//! Profile とパッチ層: どのプラグインを有効にし、どの順に並べ、どんな設定を渡すかを層で重ねて決める。
//!
//! 層は「発見したプラグイン (すべて有効・発見順) → profile のパッチ → `--patch` のパッチ」の順に重なる。
//! パッチは TOML で、行 (`[[plugins]]`) ごとに1つのプラグインを `id` (manifest の `name`) で指すか、
//! `path` で発見されていないプラグインを足す。
//!
//! ```toml
//! [[plugins]]
//! id = "guard"
//! order = -10                 # 小さいほど先 (既定 0、同じなら発見順)。Hook の割り込む順にも効く
//! config = { deny_tools = "bash" }
//!
//! [[plugins]]
//! path = "~/src/my-plugin"    # plugin.toml のあるディレクトリ。相対パスはこのファイルから
//! disabled = true
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::external::{MANIFEST_FILE, Manifest};
use crate::{Error, Result};

/// profile のパッチのファイル名。
pub const PATCH_FILE: &str = "patch.toml";

/// `--profile` を指定しないときの profile。`$LETSHARNESS_PROFILE` で変えられる。
pub const DEFAULT_PROFILE: &str = "default";

/// LetsHarness の置き場: `$LETSHARNESS_HOME` → `~/.letsharness`。
pub fn home_dir() -> Option<PathBuf> {
	if let Some(dir) = std::env::var_os("LETSHARNESS_HOME") {
		return Some(PathBuf::from(dir));
	}
	std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".letsharness"))
}

/// profile のパッチの場所: `<home>/profiles/<profile>/patch.toml`。
pub fn profile_patch_path(profile: &str) -> Result<PathBuf> {
	let valid = !profile.is_empty()
		&& profile
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
	if !valid {
		return Err(Error::Config(format!(
			"profile `{profile}`: 英数字・`-`・`_` だけが使えます"
		)));
	}
	let home =
		home_dir().ok_or_else(|| Error::Config("profile の置き場がありません ($LETSHARNESS_HOME / $HOME)".into()))?;
	Ok(home.join("profiles").join(profile).join(PATCH_FILE))
}

/// 1枚のパッチ。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub plugins: Vec<PatchRow>,
}

/// パッチの1行。書いたフィールドだけが前の層を上書きする。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRow {
	/// 対象のプラグイン (manifest の `name`)。`path` と併記すると、その manifest の名前と一致する必要がある。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<String>,
	/// 発見されていないプラグインを足す (`plugin.toml` のあるディレクトリ)。
	/// 相対パスはこのパッチのあるディレクトリから、`~/` はホームから解決する。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub path: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub disabled: Option<bool>,
	/// 並び順。小さいほど先で、既定は 0。同じなら発見順 (足したものは足した順)。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub order: Option<i64>,
	/// プラグインに渡す設定。前の層の `config` を丸ごと置き換える。`-o key=value` はこれより強い。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub config: Option<Map<String, Value>>,
}

impl Patch {
	pub fn load(path: &Path) -> Result<Self> {
		let src = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
		toml::from_str(&src).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
	}

	/// ファイルが無ければ空のパッチ。
	pub fn load_or_default(path: &Path) -> Result<Self> {
		if path.exists() {
			Self::load(path)
		} else {
			Ok(Self::default())
		}
	}

	/// 書き戻す (ディレクトリも作る)。コメントや書式は残らない。
	pub fn save(&self, path: &Path) -> Result<()> {
		let src = toml::to_string(self).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir).map_err(|e| Error::Config(format!("{}: {e}", dir.display())))?;
		}
		std::fs::write(path, src).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
	}

	/// この層で `id` を有効・無効にする。
	///
	/// `id` を指す行 (`id` で指すもの・`path` で足したもの) の `disabled` を消し、無効にするときだけ
	/// 末尾に `{ id, disabled = true }` を足す。何も書いていない行は消すので、既定 (有効) と同じなら跡が残らない。
	/// `source` はこのパッチのファイルで、`path` の解決に使う。
	pub fn set_enabled(&mut self, source: &Path, id: &str, enabled: bool) -> Result<()> {
		let base = source.parent().unwrap_or(Path::new("."));
		let mut targets = Vec::new();
		for row in &self.plugins {
			targets.push(row_target(base, row)?.as_deref() == Some(id));
		}
		let mut targets = targets.into_iter();
		self.plugins.retain_mut(|row| {
			if !targets.next().unwrap_or(false) {
				return true;
			}
			row.disabled = None;
			row.path.is_some() || row.order.is_some() || row.config.is_some()
		});
		if !enabled {
			self.plugins.push(PatchRow {
				id: Some(id.to_owned()),
				disabled: Some(true),
				..Default::default()
			});
		}
		Ok(())
	}
}

/// 行が指すプラグインの名前。`path` の行は manifest を読む。
fn row_target(base: &Path, row: &PatchRow) -> Result<Option<String>> {
	match (&row.path, &row.id) {
		(Some(path), _) => Ok(Some(Manifest::load(&resolve(base, path).join(MANIFEST_FILE))?.name)),
		(None, id) => Ok(id.clone()),
	}
}

fn resolve(base: &Path, path: &str) -> PathBuf {
	if let Some(rest) = path.strip_prefix("~/")
		&& let Some(home) = std::env::var_os("HOME")
	{
		return PathBuf::from(home).join(rest);
	}
	base.join(path)
}

/// パッチ1枚と、それがどこから来たか。
#[derive(Debug, Clone)]
pub struct Layer {
	pub source: PathBuf,
	pub patch: Patch,
}

/// 層を重ねた後の1プラグイン。
#[derive(Debug, Clone)]
pub struct PluginState {
	pub manifest: Manifest,
	pub enabled: bool,
	pub order: i64,
	/// プラグインに渡す設定 (`-o key=value` はこれより強い)。
	pub config: Map<String, Value>,
}

impl PluginState {
	fn new(manifest: Manifest) -> Self {
		Self {
			manifest,
			enabled: true,
			order: 0,
			config: Map::new(),
		}
	}
}

/// 発見したプラグインに層を順に重ね、並び順に並べる。無効なものも含めて返す。
///
/// 知らない `id`、名前の重複、`id` も `path` も無い行は、どのファイルの何行目かを添えて `Err(Config)` にする
/// (書き間違いを黙って無視しない)。
pub fn compose(discovered: Vec<Manifest>, layers: &[Layer]) -> Result<Vec<PluginState>> {
	let mut states: Vec<PluginState> = discovered.into_iter().map(PluginState::new).collect();
	for layer in layers {
		let base = layer.source.parent().unwrap_or(Path::new("."));
		for (i, row) in layer.patch.plugins.iter().enumerate() {
			let at = |msg: String| Error::Config(format!("{} (plugins[{i}]): {msg}", layer.source.display()));
			let index = match (&row.path, &row.id) {
				(Some(path), id) => {
					let manifest =
						Manifest::load(&resolve(base, path).join(MANIFEST_FILE)).map_err(|e| at(e.to_string()))?;
					if let Some(id) = id.as_deref().filter(|id| *id != manifest.name) {
						return Err(at(format!(
							"id `{id}` と manifest の name `{}` が違います",
							manifest.name
						)));
					}
					if states.iter().any(|s| s.manifest.name == manifest.name) {
						return Err(at(format!("plugin `{}` はすでにあります", manifest.name)));
					}
					states.push(PluginState::new(manifest));
					states.len() - 1
				}
				(None, Some(id)) => states.iter().position(|s| s.manifest.name == *id).ok_or_else(|| {
					let known: Vec<&str> = states.iter().map(|s| s.manifest.name.as_str()).collect();
					at(format!("unknown plugin `{id}` ({})", known.join("|")))
				})?,
				(None, None) => return Err(at("`id` か `path` が必要です".into())),
			};
			let state = &mut states[index];
			if let Some(disabled) = row.disabled {
				state.enabled = !disabled;
			}
			if let Some(order) = row.order {
				state.order = order;
			}
			if let Some(config) = &row.config {
				state.config = config.clone();
			}
		}
	}
	states.sort_by_key(|s| s.order);
	Ok(states)
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn manifest(name: &str) -> Manifest {
		toml::from_str(&format!("name = \"{name}\"\ncommand = \"x\"")).unwrap()
	}

	fn layer(src: &str) -> Layer {
		Layer {
			source: PathBuf::from("/p/patch.toml"),
			patch: toml::from_str(src).unwrap(),
		}
	}

	fn names(states: &[PluginState]) -> Vec<(&str, bool)> {
		states.iter().map(|s| (s.manifest.name.as_str(), s.enabled)).collect()
	}

	#[test]
	fn later_layers_win_and_order_sorts() {
		let profile = layer(
			r#"
			[[plugins]]
			id = "b"
			disabled = true
			config = { k = "profile", only = 1 }
			[[plugins]]
			id = "c"
			order = -1
			"#,
		);
		let overlay = layer(
			r#"
			[[plugins]]
			id = "b"
			disabled = false
			config = { k = "overlay" }
			"#,
		);
		let states = compose(vec![manifest("a"), manifest("b"), manifest("c")], &[profile, overlay]).unwrap();
		assert_eq!(names(&states), [("c", true), ("a", true), ("b", true)]);
		// config は丸ごと置き換わる
		assert_eq!(Value::Object(states[2].config.clone()), json!({ "k": "overlay" }));
	}

	#[test]
	fn mistakes_fail_loud() {
		let err = compose(vec![manifest("a")], &[layer("[[plugins]]\nid = \"nope\"")]).unwrap_err();
		assert_eq!(
			err.to_string(),
			"config: /p/patch.toml (plugins[0]): unknown plugin `nope` (a)"
		);
		assert!(compose(vec![], &[layer("[[plugins]]\ndisabled = true")]).is_err());
		assert!(toml::from_str::<Patch>("[[plugins]]\nid = \"a\"\ndisable = true").is_err());
		assert!(profile_patch_path("../x").is_err());
	}

	#[test]
	fn paths_add_plugins_relative_to_the_patch() {
		let dir = std::env::temp_dir().join(format!("lh-profile-{}", std::process::id()));
		let plugin = dir.join("extra");
		std::fs::create_dir_all(&plugin).unwrap();
		std::fs::write(plugin.join(MANIFEST_FILE), "name = \"extra\"\ncommand = \"x\"").unwrap();
		let layer = Layer {
			source: dir.join(PATCH_FILE),
			patch: toml::from_str("[[plugins]]\npath = \"extra\"").unwrap(),
		};
		let states = compose(vec![manifest("a")], std::slice::from_ref(&layer)).unwrap();
		assert_eq!(names(&states), [("a", true), ("extra", true)]);
		assert_eq!(states[1].manifest.dir, plugin);
		// 同じものを2度足すのは誤り
		assert!(compose(vec![manifest("extra")], std::slice::from_ref(&layer)).is_err());

		// path で足したものも id で切り替えられる
		let mut patch = layer.patch.clone();
		patch.set_enabled(&layer.source, "extra", false).unwrap();
		let off = Layer { patch, ..layer };
		assert_eq!(names(&compose(vec![], &[off]).unwrap()), [("extra", false)]);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn toggling_leaves_no_trace_when_back_to_default() {
		let src = Path::new("/p/patch.toml");
		let mut patch = Patch::default();
		patch.set_enabled(src, "a", false).unwrap();
		patch.set_enabled(src, "a", false).unwrap();
		assert_eq!(patch.plugins.len(), 1, "no duplicate rows");
		patch.set_enabled(src, "a", true).unwrap();
		assert_eq!(patch, Patch::default());

		// 他のフィールドを持つ行は残し、disabled だけ消す
		let mut patch: Patch = toml::from_str("[[plugins]]\nid = \"a\"\norder = 3\ndisabled = true").unwrap();
		patch.set_enabled(src, "a", true).unwrap();
		assert_eq!(patch.plugins[0].disabled, None);
		assert_eq!(patch.plugins[0].order, Some(3));
	}

	#[test]
	fn patches_round_trip_through_files() {
		let path = std::env::temp_dir()
			.join(format!("lh-patch-{}", std::process::id()))
			.join(PATCH_FILE);
		assert_eq!(Patch::load_or_default(&path).unwrap(), Patch::default());
		let patch: Patch = toml::from_str("[[plugins]]\nid = \"a\"\nconfig = { x = \"y\" }").unwrap();
		patch.save(&path).unwrap();
		assert_eq!(Patch::load(&path).unwrap(), patch);
		std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}
}
