//! 設定ファイル (`<home>/config.toml`): 使う Provider に名前を付け、既定を決める。
//!
//! ```toml
//! default_provider = "work"
//!
//! [providers.work]
//! kind = "commandcode"    # 省略すると表の名前 (ここでは "work") を kind とみなす
//! wire = "messages"       # kind 以外はそのまま Provider の cfg に渡る
//!
//! [providers.echo]
//! kind = "mock"
//! prefix = ">> "
//! ```
//!
//! 同じ kind で複数のインスタンスを持てる。`--provider` にはインスタンス名か kind を渡す。
//! Provider の cfg は「パッチ層のプラグイン設定 → ここの表 → `-o key=value`」の順に上書きされる。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Error, Result};

/// 設定ファイルのファイル名 (`<home>/config.toml`)。
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	/// `--provider` を省いたときの Provider (インスタンス名か kind)。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default_provider: Option<String>,
	/// 名前付きの Provider。
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub providers: BTreeMap<String, ProviderConfig>,
}

/// `[providers.<name>]` の表。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
	/// Provider の kind (`plugin.toml` の `contributes.provider`)。省略すると表の名前。
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub kind: Option<String>,
	/// Provider に渡す設定。
	#[serde(flatten)]
	pub cfg: Map<String, Value>,
}

impl Config {
	/// `path` を読む。無ければ空の設定。
	pub fn load_or_default(path: &Path) -> Result<Self> {
		match std::fs::read_to_string(path) {
			Ok(src) => Self::parse(&src).map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(Error::Config(format!("{}: {e}", path.display()))),
		}
	}

	fn parse(src: &str) -> std::result::Result<Self, String> {
		let config: Config = toml::from_str(src).map_err(|e| e.to_string())?;
		for (name, p) in &config.providers {
			if name.is_empty() || p.kind.as_deref() == Some("") {
				return Err(format!("providers.{name}: 名前と kind は空にできません"));
			}
		}
		if config.default_provider.as_deref() == Some("") {
			return Err("default_provider は空にできません".into());
		}
		Ok(config)
	}

	/// `name` (インスタンス名か kind) を `(kind, cfg)` にする。表に無い名前は kind とみなし、cfg は空。
	pub fn resolve(&self, name: &str) -> (String, Map<String, Value>) {
		match self.providers.get(name) {
			Some(p) => (p.kind.clone().unwrap_or_else(|| name.to_owned()), p.cfg.clone()),
			None => (name.to_owned(), Map::new()),
		}
	}

	/// 選べる Provider の名前: 表の名前 (名前順) → それ以外の `kinds`。
	pub fn provider_names(&self, kinds: &[&str]) -> Vec<String> {
		let mut names: Vec<String> = self.providers.keys().cloned().collect();
		names.extend(
			kinds
				.iter()
				.filter(|k| !self.providers.contains_key(**k))
				.map(|k| (*k).to_owned()),
		);
		names
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn names_resolve_to_kinds_and_cfg() {
		let config = Config::parse(
			r#"
default_provider = "work"
[providers.work]
kind = "commandcode"
wire = "messages"
[providers.mock]
prefix = ">> "
"#,
		)
		.unwrap();
		assert_eq!(config.default_provider.as_deref(), Some("work"));
		let (kind, cfg) = config.resolve("work");
		assert_eq!(
			(kind.as_str(), Value::Object(cfg)),
			("commandcode", json!({ "wire": "messages" }))
		);
		assert_eq!(config.resolve("mock").0, "mock", "the table name is the kind");
		assert_eq!(config.resolve("gemini"), ("gemini".into(), Map::new()));
		assert_eq!(
			config.provider_names(&["commandcode", "mock", "gemini"]),
			["mock", "work", "commandcode", "gemini"]
		);
	}

	#[test]
	fn mistakes_fail_loud() {
		assert!(Config::parse("default_providr = \"x\"").is_err());
		assert!(Config::parse("[providers.x]\nkind = \"\"").is_err());
		let missing = std::env::temp_dir().join(format!("lh-no-config-{}.toml", std::process::id()));
		assert_eq!(Config::load_or_default(&missing).unwrap(), Config::default());
	}
}
