//! テスト・オフライン用のモック実装.
//!
//! ネットワークに触れず [`Provider`] を満たすため、
//! ハーネスのロジック (ルーティング・表示・ツールループ) のテストに使える。

use letsharness_kernel::{ChatMessage, Error, ModelInfo, Provider, Result};

#[derive(Debug, Clone, Default)]
pub struct MockClient {
	pub echo_prefix: String,
	/// Someにすると全リクエストが [`Error::Plugin`] で失敗する (異常系テスト用)。
	pub fail_with: Option<String>,
}

impl MockClient {
	pub fn new() -> Self {
		Self {
			echo_prefix: "[mock] ".to_string(),
			fail_with: None,
		}
	}

	pub fn with_prefix(prefix: impl Into<String>) -> Self {
		Self {
			echo_prefix: prefix.into(),
			fail_with: None,
		}
	}

	pub fn with_error(msg: impl Into<String>) -> Self {
		Self {
			echo_prefix: "[mock] ".to_string(),
			fail_with: Some(msg.into()),
		}
	}

	fn fail(&self) -> Option<Error> {
		self.fail_with.clone().map(|msg| Error::plugin("mock", msg))
	}
}

#[async_trait::async_trait]
impl Provider for MockClient {
	fn name(&self) -> &'static str {
		"mock"
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		Ok(vec![
			ModelInfo {
				id: "mock/mock-model".to_string(),
				name: "Mock Model".to_string(),
				context_length: 8192,
				supported_endpoints: Some(vec!["/chat/completions".to_string()]),
			},
			ModelInfo {
				id: "claude-mock".to_string(),
				name: "Mock Claude".to_string(),
				context_length: 8192,
				supported_endpoints: Some(vec!["/messages".to_string()]),
			},
		])
	}

	async fn chat(&self, model: &str, messages: Vec<ChatMessage>, _max_tokens: Option<u32>) -> Result<String> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		let last = messages.last().map(|m| m.content.as_str()).unwrap_or("");
		Ok(format!("{}{model}: {last}", self.echo_prefix))
	}

	async fn chat_stream(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		let full = self.chat(model, messages, None).await?;
		// モックは一括→分割配送でストリームを模す
		for chunk in full.split_inclusive(' ') {
			on_delta(chunk);
		}
		Ok(full)
	}

	async fn responses(&self, model: &str, input: &str) -> Result<String> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		Ok(format!("{}{model}: {input}", self.echo_prefix))
	}

	async fn messages(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		_system: Option<&str>,
		_max_tokens: u32,
	) -> Result<String> {
		self.chat(model, messages, None).await
	}
}
