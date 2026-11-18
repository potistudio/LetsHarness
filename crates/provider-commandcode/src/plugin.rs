//! kernel への登録。認証は環境変数 (`COMMAND_CODE_API_KEY` など) から読む。
//! `cfg.wire` (`auto` | `chat` | `responses` | `messages`) でリクエストに指定が無いときの wire を決め打ちできる。

use std::sync::Arc;

use letsharness_kernel::{PluginServer, Provider, ProviderFactory, Result};
use serde_json::Value;

use crate::{CommandCodeProvider, Wire};

pub fn server() -> PluginServer {
	PluginServer::new().provider(CommandCodeFactory)
}

struct CommandCodeFactory;

impl ProviderFactory for CommandCodeFactory {
	fn kind(&self) -> &'static str {
		"commandcode"
	}

	fn create(&self, cfg: &Value) -> Result<Arc<dyn Provider>> {
		let wire = match cfg.get("wire").and_then(Value::as_str) {
			Some(w) => Wire::parse(w)?,
			None => None,
		};
		Ok(Arc::new(CommandCodeProvider::from_env()?.with_wire(wire)))
	}
}
