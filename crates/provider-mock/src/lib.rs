//! テスト・オフライン用のモック実装.
//!
//! ネットワークに触れず [`Provider`] を満たすため、
//! ハーネスのロジック (ルーティング・表示・ツールループ) のテストに使える。

use letsharness_kernel::{
	ChatMessage, ChatOutput, Completion, Error, ModelInfo, Provider, Quota, QuotaWindow, Result, ToolCall, ToolSpec,
	Usage,
};

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

	/// テスト用思考過程。モデル名に `think` を含む場合のみ返す。
	/// 例: `--provider mock --model mock-think` で Thinking 表示を確認できる。
	fn mock_thinking(model: &str) -> Option<String> {
		if model.contains("think") {
			Some(format!("mock thinking for {model}"))
		} else {
			None
		}
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
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<String> {
		let full = self.chat(model, messages, None).await?;
		// モックは一括→分割配送でストリームを模す (thinking も同様)
		if let Some(thinking) = Self::mock_thinking(model) {
			for chunk in thinking.split_inclusive(' ') {
				on_thinking(chunk);
			}
		}
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

	async fn quota(&self) -> Result<Quota> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		Ok(Quota {
			plan: Some("mock".to_string()),
			status: Some("active".to_string()),
			credit_balance: Some(100.0),
			monthly_remaining: Some(100.0),
			purchased_credits: Some(0.0),
			free_credits: Some(0.0),
			five_hour: Some(QuotaWindow::new(1.0, 14.0, None, false)),
			weekly: Some(QuotaWindow::new(2.0, 35.0, None, false)),
			monthly: Some(QuotaWindow::new(0.0, 100.0, None, false)),
			period_spend: Some(2.0),
			period_tokens: Some(1000),
			extra: serde_json::json!({"mock": true}),
		})
	}

	async fn chat_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
	) -> Result<ChatOutput> {
		let text = self.chat(model, messages.clone(), max_tokens).await?;
		let prompt = messages.iter().map(|m| m.content.len() as u64).sum::<u64>();
		Ok(ChatOutput {
			text,
			thinking: Self::mock_thinking(model),
			usage: Some(Usage {
				prompt_tokens: prompt,
				completion_tokens: 3,
				total_tokens: prompt + 3,
				..Default::default()
			}),
		})
	}

	async fn chat_stream_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let mut thinking_buf = String::new();
		let text = {
			let buf = &mut thinking_buf;
			let wrapped: &mut (dyn for<'a> FnMut(&'a str) + Send) = &mut |chunk: &str| {
				buf.push_str(chunk);
				on_thinking(chunk);
			};
			self.chat_stream(model, messages.clone(), on_delta, wrapped).await?
		};
		let prompt = messages.iter().map(|m| m.content.len() as u64).sum::<u64>();
		Ok(ChatOutput {
			text,
			thinking: ChatOutput::normalized_thinking(Some(thinking_buf)),
			usage: Some(Usage {
				prompt_tokens: prompt,
				completion_tokens: 3,
				total_tokens: prompt + 3,
				..Default::default()
			}),
		})
	}

	/// 台本ツール呼び出し。直近が user で本文が `tool:<name> <JSON引数>` なら
	/// そのツールを呼ぶ。直近が tool 結果なら結果を引用して終える。それ以外はエコー。
	/// ネットワーク無しでエージェントループを通せる。
	async fn chat_with_tools(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		tools: &[ToolSpec],
		max_tokens: Option<u32>,
	) -> Result<Completion> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		let last = messages.last();
		if let Some(m) = last.filter(|m| m.role == "tool") {
			return Ok(Completion {
				text: format!("{}tool result: {}", self.echo_prefix, m.content),
				..Default::default()
			});
		}
		if !tools.is_empty()
			&& let Some(rest) = last
				.filter(|m| m.role == "user")
				.and_then(|m| m.content.strip_prefix("tool:"))
		{
			let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
			let arguments = if args.trim().is_empty() {
				serde_json::json!({})
			} else {
				serde_json::from_str(args)
					.map_err(|e| Error::plugin("mock", format!("tool: の引数がJSONではない: {e}")))?
			};
			return Ok(Completion {
				text: format!("{}calling {name}", self.echo_prefix),
				tool_calls: vec![ToolCall {
					id: format!("mock_call_{}", messages.len()),
					name: name.to_string(),
					arguments,
				}],
				..Default::default()
			});
		}
		let out = self.chat_with_usage(model, messages, max_tokens).await?;
		Ok(Completion {
			text: out.text,
			thinking: out.thinking,
			tool_calls: Vec::new(),
			usage: out.usage,
		})
	}

	async fn responses_with_usage(&self, model: &str, input: &str) -> Result<ChatOutput> {
		let text = self.responses(model, input).await?;
		Ok(ChatOutput {
			text,
			thinking: Self::mock_thinking(model),
			usage: None,
		})
	}

	async fn messages_with_usage(
		&self,
		model: &str,
		messages: Vec<ChatMessage>,
		system: Option<&str>,
		max_tokens: u32,
	) -> Result<ChatOutput> {
		let text = self.messages(model, messages, system, max_tokens).await?;
		Ok(ChatOutput {
			text,
			thinking: Self::mock_thinking(model),
			usage: None,
		})
	}
}
