//! kernel への登録。認証は環境変数 (`GEMINI_API_KEY` など) から読むので `cfg` は使わない。

use std::sync::Arc;

use letsharness_kernel::{PluginServer, Provider, ProviderFactory, Result};
use serde_json::Value;

use crate::GeminiProvider;

pub fn server() -> PluginServer {
	PluginServer::new().provider(GeminiFactory)
}

struct GeminiFactory;

impl ProviderFactory for GeminiFactory {
	fn kind(&self) -> &'static str {
		"gemini"
	}

	fn create(&self, _cfg: &Value) -> Result<Arc<dyn Provider>> {
		Ok(Arc::new(GeminiProvider::from_env()?))
	}
}
