//! kernel への登録。`cfg.workdir` (既定 `.`) を作業ディレクトリにして同梱ツールを供給する。

use std::sync::Arc;

use letsharness_kernel::{Plugin, Registry, Result, Tool, ToolFactory};
use serde_json::Value;

use crate::builtin_tools;

pub struct BuiltinToolsPlugin;

impl Plugin for BuiltinToolsPlugin {
	fn name(&self) -> &'static str {
		"tool-builtin"
	}

	fn register(&self, registry: &mut Registry) -> Result<()> {
		registry.add::<dyn ToolFactory>(Arc::new(BuiltinToolFactory));
		Ok(())
	}
}

struct BuiltinToolFactory;

impl ToolFactory for BuiltinToolFactory {
	fn name(&self) -> &'static str {
		"builtin"
	}

	fn create(&self, cfg: &Value) -> Result<Vec<Arc<dyn Tool>>> {
		let workdir = cfg.get("workdir").and_then(Value::as_str).unwrap_or(".");
		Ok(builtin_tools(workdir))
	}
}
