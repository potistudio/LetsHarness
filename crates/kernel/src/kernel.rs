//! Kernel の起動。プラグインを発見してパッチ層を重ね、`boot` を呼んでから `Host` を返す。
//!
//! Client は `Kernel::boot` を呼ぶだけで、どのプラグインがあるかは知らない。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::external::{Connection, PluginEntry, default_plugin_path, discover};
use crate::profile::{DEFAULT_PROFILE, Layer, Patch, PluginState, compose, profile_patch_path};
use crate::{Error, Host, Result};

pub struct BootConfig {
	/// `-o key=value`。`boot` を持つプラグインにそのまま渡る (例: dotenv の `env_file`)。
	pub options: Vec<(String, String)>,
	/// プラグインの探索先。
	pub plugin_path: Vec<PathBuf>,
	/// 最初に重ねるパッチ層の profile (`<home>/profiles/<profile>/patch.toml`、無ければ空)。
	/// `None` なら profile の層を使わない。
	pub profile: Option<String>,
	/// profile の後に順に重ねるパッチ (`--patch`)。ファイルが無ければ起動を止める。
	pub patches: Vec<PathBuf>,
}

impl BootConfig {
	/// 探索先は [`default_plugin_path`]、profile は `$LETSHARNESS_PROFILE` (無ければ [`DEFAULT_PROFILE`])。
	pub fn new(options: Vec<(String, String)>) -> Self {
		Self {
			options,
			plugin_path: default_plugin_path(),
			profile: Some(std::env::var("LETSHARNESS_PROFILE").unwrap_or_else(|_| DEFAULT_PROFILE.into())),
			patches: Vec::new(),
		}
	}

	/// profile のパッチの場所。profile を使わないなら `None`。
	pub fn profile_patch(&self) -> Result<Option<PathBuf>> {
		self.profile.as_deref().map(profile_patch_path).transpose()
	}
}

pub struct Kernel;

impl Kernel {
	/// プラグインを発見してパッチ層を重ねた結果 (無効なものも含む)。プラグインは起動しない。
	pub fn plugins(config: &BootConfig) -> Result<Vec<PluginState>> {
		let mut layers = Vec::new();
		if let Some(source) = config.profile_patch()? {
			layers.push(Layer {
				patch: Patch::load_or_default(&source)?,
				source,
			});
		}
		for source in &config.patches {
			layers.push(Layer {
				patch: Patch::load(source)?,
				source: source.clone(),
			});
		}
		compose(discover(&config.plugin_path), &layers)
	}

	/// 有効なプラグインを並び順に用意し、`boot` を持つものを順に呼び、返された環境変数を以降に起動するプラグインへ渡す。
	///
	/// Kernel 自身は環境変数を書き換えない。
	pub async fn boot(config: BootConfig) -> Result<Host> {
		let plugins: Vec<PluginState> = Self::plugins(&config)?.into_iter().filter(|p| p.enabled).collect();
		let options: Value = Value::Object(config.options.iter().map(|(k, v)| (k.clone(), json!(v))).collect());

		let mut env = HashMap::new();
		for plugin in plugins.iter().filter(|p| p.manifest.contributes.boot) {
			let entry = PluginEntry::new(plugin.manifest.clone(), plugin.config.clone(), Arc::new(HashMap::new()));
			let conn = Connection::spawn(&entry.manifest, entry.name, &HashMap::new()).await?;
			let booted = conn.call("boot", json!({ "options": entry.cfg(&options) })).await?;
			let vars = booted.get("env").cloned().unwrap_or(Value::Null);
			let vars: HashMap<String, String> =
				serde_json::from_value(vars).map_err(|e| Error::plugin(entry.name, e))?;
			env.extend(vars);
		}

		let env = Arc::new(env);
		let entries = plugins
			.into_iter()
			.map(|p| Arc::new(PluginEntry::new(p.manifest, p.config, env.clone())))
			.collect();
		Ok(Host::new(entries))
	}
}
