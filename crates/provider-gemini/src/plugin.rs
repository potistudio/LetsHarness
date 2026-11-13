//! kernel への登録。認証は環境変数 (`GEMINI_API_KEY` など) から読むので `cfg` は使わない。

use std::sync::Arc;

use letsharness_kernel::{Plugin, Provider, ProviderFactory, Registry, Result};
use serde_json::Value;

use crate::GeminiProvider;

pub struct GeminiPlugin;

impl Plugin for GeminiPlugin {
	fn name(&self) -> &'static str {
		"provider-gemini"
	}

	fn register(&self, registry: &mut Registry) -> Result<()> {
		registry.add::<dyn ProviderFactory>(Arc::new(GeminiFactory));
		Ok(())
	}
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
