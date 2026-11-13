//! kernel への登録。`cfg` は `{"prefix": "...", "error": "..."}` (どちらも省略可)。

use std::sync::Arc;

use letsharness_kernel::{PluginServer, Provider, ProviderFactory, Result};
use serde_json::Value;

use crate::MockClient;

pub fn server() -> PluginServer {
	PluginServer::new().provider(MockFactory)
}

struct MockFactory;

impl ProviderFactory for MockFactory {
	fn kind(&self) -> &'static str {
		"mock"
	}

	fn create(&self, cfg: &Value) -> Result<Arc<dyn Provider>> {
		let str_of = |key: &str| cfg.get(key).and_then(Value::as_str).map(str::to_owned);
		let client = match (str_of("error"), str_of("prefix")) {
			(Some(msg), _) => MockClient::with_error(msg),
			(None, Some(prefix)) if !prefix.is_empty() => MockClient::with_prefix(prefix),
			_ => MockClient::new(),
		};
		Ok(Arc::new(client))
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[tokio::test]
	async fn factory_applies_cfg() {
		let chat = |p: Arc<dyn Provider>| async move {
			p.chat("m", vec![letsharness_kernel::ChatMessage::user("hi")], None)
				.await
				.map_err(|e| e.to_string())
		};
		let f = MockFactory;
		assert_eq!(chat(f.create(&json!({})).unwrap()).await.unwrap(), "[mock] m: hi");
		assert_eq!(
			chat(f.create(&json!({"prefix": ">> "})).unwrap()).await.unwrap(),
			">> m: hi"
		);
		assert!(
			chat(f.create(&json!({"error": "boom"})).unwrap())
				.await
				.unwrap_err()
				.contains("boom")
		);
	}
}
