//! kernel への登録。`cfg.workdir` があれば `agy` の作業ディレクトリにする。

use std::sync::Arc;

use letsharness_kernel::{AgentBackend, AgentBackendFactory, Plugin, Registry, Result};
use serde_json::Value;

use crate::AntigravityAgent;

pub struct AntigravityPlugin;

impl Plugin for AntigravityPlugin {
	fn name(&self) -> &'static str {
		"agent-antigravity"
	}

	fn register(&self, registry: &mut Registry) -> Result<()> {
		registry.add::<dyn AgentBackendFactory>(Arc::new(AntigravityFactory));
		Ok(())
	}
}

struct AntigravityFactory;

impl AgentBackendFactory for AntigravityFactory {
	fn kind(&self) -> &'static str {
		"antigravity"
	}

	fn aliases(&self) -> &'static [&'static str] {
		&["agy"]
	}

	fn create(&self, cfg: &Value) -> Result<Arc<dyn AgentBackend>> {
		let mut agent = AntigravityAgent::from_env();
		if let Some(dir) = cfg.get("workdir").and_then(Value::as_str) {
			agent = agent.with_workdir(dir);
		}
		Ok(Arc::new(agent))
	}
}
