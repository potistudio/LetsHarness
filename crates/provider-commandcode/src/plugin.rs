//! kernel への登録。認証は環境変数 (`COMMAND_CODE_API_KEY` など) から読むので `cfg` は使わない。

use std::sync::Arc;

use letsharness_kernel::{Plugin, Provider, ProviderFactory, Registry, Result};
use serde_json::Value;

use crate::CommandCodeProvider;

pub struct CommandCodePlugin;

impl Plugin for CommandCodePlugin {
	fn name(&self) -> &'static str {
		"provider-commandcode"
	}

	fn register(&self, registry: &mut Registry) -> Result<()> {
		registry.add::<dyn ProviderFactory>(Arc::new(CommandCodeFactory));
		Ok(())
	}
}

struct CommandCodeFactory;

impl ProviderFactory for CommandCodeFactory {
	fn kind(&self) -> &'static str {
		"commandcode"
	}

	fn create(&self, _cfg: &Value) -> Result<Arc<dyn Provider>> {
		Ok(Arc::new(CommandCodeProvider::from_env()?))
	}
}
