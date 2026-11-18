//! テスト・オフライン用のモック実装.
//!
//! ネットワークに触れず [`Provider`] を満たすため、
//! ハーネスのロジック (ルーティング・表示・ツールループ) のテストに使える。

mod plugin;

pub use plugin::server;

use letsharness_kernel::{
	ChatRequest, ChatSink, ChatStream, Completion, Error, ModelInfo, Provider, Quota, QuotaWindow, Result, ToolCall,
	Usage, chat_stream,
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

	/// 台本ツール呼び出し。ツールがあって直近が user で本文が `tool:<name> <JSON引数>` なら
	/// そのツールを呼ぶ。直近が tool 結果なら結果を引用して終える。それ以外はエコー
	/// (`<prefix><model>: <最後の本文>`)。ネットワーク無しでエージェントループを通せる。
	///
	/// 本文・思考過程は空白ごとに分けて差分として流す (ストリームを模す)。
	fn respond(&self, model: &str, req: ChatRequest, sink: &ChatSink) -> Result<Completion> {
		if let Some(e) = self.fail() {
			return Err(e);
		}
		let messages = &req.messages;
		let last = messages.last();
		if let Some(m) = last.filter(|m| m.role == "tool") {
			let text = format!("{}tool result: {}", self.echo_prefix, m.content);
			sink.text(&text);
			return Ok(Completion {
				text,
				..Default::default()
			});
		}
		if !req.tools.is_empty()
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
			let text = format!("{}calling {name}", self.echo_prefix);
			sink.text(&text);
			return Ok(Completion {
				text,
				tool_calls: vec![ToolCall::new(format!("mock_call_{}", messages.len()), name, arguments)],
				..Default::default()
			});
		}
		let thinking = Self::mock_thinking(model);
		for chunk in thinking.iter().flat_map(|t| t.split_inclusive(' ')) {
			sink.thinking(chunk);
		}
		let text = format!(
			"{}{model}: {}",
			self.echo_prefix,
			last.map(|m| m.content.as_str()).unwrap_or("")
		);
		for chunk in text.split_inclusive(' ') {
			sink.text(chunk);
		}
		let prompt = messages.iter().map(|m| m.content.len() as u64).sum::<u64>();
		Ok(Completion {
			text,
			thinking,
			tool_calls: Vec::new(),
			usage: Some(Usage {
				prompt_tokens: prompt,
				completion_tokens: 3,
				total_tokens: prompt + 3,
				..Default::default()
			}),
		})
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

	fn chat<'a>(&'a self, model: &'a str, req: ChatRequest) -> ChatStream<'a> {
		chat_stream(move |sink| async move { self.respond(model, req, &sink) })
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
}
