//! kernel への登録。`cfg.workdir` (既定 `.`) を作業ディレクトリにして同梱ツールを供給する。

use std::sync::Arc;

use letsharness_kernel::{PluginServer, Result, Tool, ToolFactory};
use serde_json::Value;

use crate::local_tools;

pub fn server() -> PluginServer {
	PluginServer::new().tool(LocalToolFactory)
}

struct LocalToolFactory;

impl ToolFactory for LocalToolFactory {
	fn name(&self) -> &'static str {
		"local"
	}

	fn create(&self, cfg: &Value) -> Result<Vec<Arc<dyn Tool>>> {
		let workdir = cfg.get("workdir").and_then(Value::as_str).unwrap_or(".");
		Ok(local_tools(workdir))
	}
}
