//! Kernel の起動。プラグインを発見し、`boot` を呼んでから `Host` を返す。
//!
//! Client は `Kernel::boot` を呼ぶだけで、どのプラグインがあるかは知らない。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::external::{Connection, PluginEntry, default_plugin_path, discover};
use crate::{Error, Host, Result};

pub struct BootConfig {
	/// `-o key=value`。`boot` を持つプラグインにそのまま渡る (例: dotenv の `env_file`)。
	pub options: Vec<(String, String)>,
	/// プラグインの探索先。
	pub plugin_path: Vec<PathBuf>,
}

impl BootConfig {
	/// 探索先は [`default_plugin_path`]。
	pub fn new(options: Vec<(String, String)>) -> Self {
		Self {
			options,
			plugin_path: default_plugin_path(),
		}
	}
}

pub struct Kernel;

impl Kernel {
	/// プラグインを発見し、`boot` を持つものを順に呼び、返された環境変数を以降に起動するプラグインへ渡す。
	///
	/// Kernel 自身は環境変数を書き換えない。
	pub async fn boot(config: BootConfig) -> Result<Host> {
		let manifests = discover(&config.plugin_path);
		let options: Map<String, Value> = config.options.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

		let mut env = HashMap::new();
		for manifest in manifests.iter().filter(|m| m.contributes.boot) {
			let entry = PluginEntry::new(manifest.clone(), Arc::new(HashMap::new()));
			let conn = Connection::spawn(&entry.manifest, entry.name, &HashMap::new()).await?;
			let booted = conn.call("boot", json!({ "options": options })).await?;
			let vars = booted.get("env").cloned().unwrap_or(Value::Null);
			let vars: HashMap<String, String> =
				serde_json::from_value(vars).map_err(|e| Error::plugin(entry.name, e))?;
			env.extend(vars);
		}

		let env = Arc::new(env);
		let entries = manifests
			.into_iter()
			.map(|m| Arc::new(PluginEntry::new(m, env.clone())))
			.collect();
		Ok(Host::new(entries))
	}
}
