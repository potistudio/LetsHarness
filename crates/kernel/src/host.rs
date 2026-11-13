//! 拡張点の登録先 (`Registry`) と、プラグインを使う側の窓口 (`Host`)。
//!
//! プラグイン同士は互いを知らない。Client (Frontend) も Provider もツールも、
//! `Plugin::register` で `Registry` に載り、使うときは kernel の `Host` を通る。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;

use crate::{AgentBackend, Error, Provider, Result, Tool, ToolRegistry};

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// `TypeId` をキーにした汎用の入れ物。拡張点は kernel が決め打ちせず、
/// `dyn Trait` を型パラメータにして好きな trait を登録できる。
#[derive(Default)]
pub struct Registry {
	map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Registry {
	pub fn new() -> Self {
		Self::default()
	}

	/// 拡張点 `T` (例: `dyn ProviderFactory`) に登録する。登録順を保つ。
	pub fn add<T: ?Sized + Send + Sync + 'static>(&mut self, item: Arc<T>) -> &mut Self {
		self.map
			.entry(TypeId::of::<T>())
			.or_insert_with(|| Box::new(Vec::<Arc<T>>::new()))
			.downcast_mut::<Vec<Arc<T>>>()
			.expect("TypeId が一致するので同じ型")
			.push(item);
		self
	}

	/// 拡張点 `T` に登録されたものを登録順に返す。
	pub fn all<T: ?Sized + Send + Sync + 'static>(&self) -> &[Arc<T>] {
		self.map
			.get(&TypeId::of::<T>())
			.and_then(|b| b.downcast_ref::<Vec<Arc<T>>>())
			.map(Vec::as_slice)
			.unwrap_or(&[])
	}
}

// ---------------------------------------------------------------------------
// 拡張点
// ---------------------------------------------------------------------------

/// CLI / TUI / Server などのユーザー接点。Client もこの拡張点を通して載るプラグイン。
pub trait Frontend: Send + Sync {
	/// `--frontend <name>` で選ぶときの名前。
	fn name(&self) -> &'static str;

	/// `args` はプラグインが `early_init` で抜き取った後の引数 (プログラム名を含む)。
	fn run(&self, host: &Host, args: Vec<OsString>) -> ExitCode;
}

/// [`Provider`] (モデル) の「種類」。`cfg` でインスタンスを作る。
pub trait ProviderFactory: Send + Sync {
	/// 例: `"commandcode"`。
	fn kind(&self) -> &'static str;

	/// `cfg` は JSON オブジェクト (`{}` なら既定)。
	fn create(&self, cfg: &Value) -> Result<Arc<dyn Provider>>;
}

/// 外部エージェント ([`AgentBackend`]) の「種類」。
pub trait AgentBackendFactory: Send + Sync {
	/// 例: `"antigravity"`。
	fn kind(&self) -> &'static str;

	/// `kind` の別名 (例: `"agy"`)。
	fn aliases(&self) -> &'static [&'static str] {
		&[]
	}

	/// `cfg.workdir` があれば作業ディレクトリ。
	fn create(&self, cfg: &Value) -> Result<Arc<dyn AgentBackend>>;
}

/// ツールの供給元。`cfg.workdir` があれば作業ディレクトリ。
pub trait ToolFactory: Send + Sync {
	fn name(&self) -> &'static str;

	fn create(&self, cfg: &Value) -> Result<Vec<Arc<dyn Tool>>>;
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// プラグインを使う側 (Frontend など) が kernel に頼む窓口。
pub struct Host {
	registry: Registry,
	boot_help: String,
}

impl Host {
	pub fn new(registry: Registry) -> Self {
		Self {
			registry,
			boot_help: String::new(),
		}
	}

	/// プラグインが起動前に抜き取るフラグの説明を持たせる。
	pub fn with_boot_help(mut self, help: impl Into<String>) -> Self {
		self.boot_help = help.into();
		self
	}

	/// 起動前に抜き取られるフラグの説明 (Frontend の `--help` に添える)。
	pub fn boot_help(&self) -> &str {
		&self.boot_help
	}

	pub fn registry(&self) -> &Registry {
		&self.registry
	}

	/// `kind` の Provider を作る。
	pub fn provider(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn Provider>> {
		match self
			.registry
			.all::<dyn ProviderFactory>()
			.iter()
			.find(|f| f.kind() == kind)
		{
			Some(f) => f.create(cfg),
			None => Err(unknown("provider", kind, self.provider_kinds())),
		}
	}

	pub fn provider_kinds(&self) -> Vec<&'static str> {
		self.registry
			.all::<dyn ProviderFactory>()
			.iter()
			.map(|f| f.kind())
			.collect()
	}

	/// `kind` (または別名) の外部エージェントを作る。
	pub fn agent_backend(&self, kind: &str, cfg: &Value) -> Result<Arc<dyn AgentBackend>> {
		match self.find_agent_factory(kind) {
			Some(f) => f.create(cfg),
			None => Err(unknown("backend", kind, self.agent_backend_kinds())),
		}
	}

	pub fn has_agent_backend(&self, kind: &str) -> bool {
		self.find_agent_factory(kind).is_some()
	}

	pub fn agent_backend_kinds(&self) -> Vec<&'static str> {
		self.registry
			.all::<dyn AgentBackendFactory>()
			.iter()
			.map(|f| f.kind())
			.collect()
	}

	/// 登録されたすべての [`ToolFactory`] のツールを集める。
	pub fn tools(&self, cfg: &Value) -> Result<ToolRegistry> {
		let mut tools = ToolRegistry::new();
		for factory in self.registry.all::<dyn ToolFactory>() {
			tools.extend(factory.create(cfg)?);
		}
		Ok(tools)
	}

	fn find_agent_factory(&self, kind: &str) -> Option<&Arc<dyn AgentBackendFactory>> {
		self.registry
			.all::<dyn AgentBackendFactory>()
			.iter()
			.find(|f| f.kind() == kind || f.aliases().contains(&kind))
	}
}

fn unknown(what: &str, kind: &str, known: Vec<&'static str>) -> Error {
	Error::Config(format!("unknown {what}: {kind} ({})", known.join("|")))
}

#[cfg(test)]
mod tests {
	use super::*;

	trait Greeter: Send + Sync {
		fn hi(&self) -> String;
	}
	struct En;
	impl Greeter for En {
		fn hi(&self) -> String {
			"hello".into()
		}
	}

	#[test]
	fn registry_is_keyed_by_trait_and_keeps_order() {
		let mut reg = Registry::new();
		assert!(reg.all::<dyn Greeter>().is_empty());
		reg.add::<dyn Greeter>(Arc::new(En));
		reg.add::<dyn Greeter>(Arc::new(En));
		reg.add::<String>(Arc::new("x".into()));
		assert_eq!(reg.all::<dyn Greeter>().len(), 2);
		assert_eq!(reg.all::<dyn Greeter>()[0].hi(), "hello");
		assert_eq!(reg.all::<String>().len(), 1);
	}

	#[test]
	fn host_reports_unknown_kinds_with_the_known_ones() {
		let host = Host::new(Registry::new());
		let err = host.provider("nope", &Value::Null).err().unwrap();
		assert!(matches!(err, Error::Config(m) if m == "unknown provider: nope ()"));
		assert!(!host.has_agent_backend("agy"));
		assert!(host.tools(&Value::Null).unwrap().is_empty());
	}
}
