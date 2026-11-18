//! Session: Client がモデルを使うときの窓口。
//!
//! Provider・ツール・Hook・Observer をひとまとめに持ち、1回のチャットにもループにも同じ Hook を効かせる。
//! Client は [`Host::session`](crate::Host::session) で作り、Provider を直接は触らない
//! (どの Client から使っても Hook を素通りさせないため)。

use std::sync::Arc;

use crate::{
	ChatMessage, ChatOutput, Completion, Error, HookRegistry, LoopOutcome, LoopRequest, ModelInfo, ModelRequest,
	Provider, Quota, Result, RunHandler, ToolRegistry, run_loop,
};

/// [`Session::chat`] で Provider のどの呼び方を使うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
	/// `chat/completions` (非ストリーム)。
	Chat,
	/// `chat/completions` のストリーム。差分は Hook を通す前に流れる。
	Stream,
	/// `responses`。メッセージの本文を改行でつないだものを入力にする。
	Responses,
	/// `messages` (Anthropic)。最初の `system` メッセージを `system` にする。`max_tokens` が必要。
	Messages,
}

pub struct Session {
	provider: Arc<dyn Provider>,
	tools: ToolRegistry,
	hooks: HookRegistry,
}

impl Session {
	/// 部品から作る (in-process で使うとき・テスト用)。通常は [`Host::session`](crate::Host::session)。
	pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, hooks: HookRegistry) -> Self {
		Self { provider, tools, hooks }
	}

	/// Provider の名前。
	pub fn provider_name(&self) -> &'static str {
		self.provider.name()
	}

	pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		self.provider.list_models().await
	}

	pub async fn quota(&self) -> Result<Quota> {
		self.provider.quota().await
	}

	/// ツールを使わない1回のモデル呼び出し。`before_request` / `after_response` の Hook を通し、Observer に知らせる。
	///
	/// Hook が足したツールやツール呼び出しは使わない (ツールを使うなら [`run_loop`](Self::run_loop))。
	/// [`Route::Stream`] の差分は Hook を通す前の本文で、Hook の書き換えは戻り値にだけ効く。
	pub async fn chat(
		&self,
		route: Route,
		model: &str,
		messages: Vec<ChatMessage>,
		max_tokens: Option<u32>,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let req = ModelRequest {
			model: model.to_owned(),
			messages,
			tools: Vec::new(),
			max_tokens,
		};
		let out = self.chat_once(route, req, on_delta, on_thinking).await;
		self.hooks.flush().await;
		out
	}

	async fn chat_once(
		&self,
		route: Route,
		req: ModelRequest,
		on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
		on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
	) -> Result<ChatOutput> {
		let ModelRequest {
			model,
			messages,
			max_tokens,
			..
		} = self.hooks.before_request(req).await?;
		let p = &self.provider;
		let out = match route {
			Route::Chat => p.chat_with_usage(&model, messages, max_tokens).await?,
			Route::Stream => {
				p.chat_stream_with_usage(&model, messages, on_delta, on_thinking)
					.await?
			}
			Route::Responses => {
				let input = messages
					.iter()
					.map(|m| m.content.as_str())
					.collect::<Vec<_>>()
					.join("\n");
				p.responses_with_usage(&model, &input).await?
			}
			Route::Messages => {
				let max = max_tokens.ok_or_else(|| Error::Config("messages には max_tokens が必要です".into()))?;
				let system = messages.iter().find(|m| m.role == "system").map(|m| m.content.clone());
				p.messages_with_usage(&model, messages, system.as_deref(), max).await?
			}
		};
		let completion = self
			.hooks
			.after_response(Completion {
				text: out.text,
				thinking: out.thinking,
				tool_calls: Vec::new(),
				usage: out.usage,
			})
			.await?;
		Ok(ChatOutput {
			text: completion.text,
			thinking: completion.thinking,
			usage: completion.usage,
		})
	}

	/// ツールを使いながら最終回答までループを回す ([`run_loop`])。
	pub async fn run_loop(&self, req: LoopRequest, handler: &mut dyn RunHandler) -> Result<LoopOutcome> {
		let out = run_loop(&*self.provider, &self.tools, &self.hooks, handler, req).await;
		self.hooks.flush().await;
		out
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;
	use crate::{Event, Hook, HookPoint, Observer};

	/// 受け取ったメッセージ数と最後の本文を返す Provider。
	struct Echo;

	#[async_trait::async_trait]
	impl Provider for Echo {
		fn name(&self) -> &'static str {
			"echo"
		}
		async fn list_models(&self) -> Result<Vec<ModelInfo>> {
			Ok(vec![])
		}
		async fn chat(&self, _: &str, messages: Vec<ChatMessage>, _: Option<u32>) -> Result<String> {
			Ok(format!("{}:{}", messages.len(), messages.last().unwrap().content))
		}
		async fn chat_stream(
			&self,
			model: &str,
			messages: Vec<ChatMessage>,
			on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
			_: &mut (dyn for<'a> FnMut(&'a str) + Send),
		) -> Result<String> {
			let text = self.chat(model, messages, None).await?;
			on_delta(&text);
			Ok(text)
		}
		async fn responses(&self, _: &str, input: &str) -> Result<String> {
			Ok(format!("responses:{input}"))
		}
		async fn messages(&self, _: &str, _: Vec<ChatMessage>, system: Option<&str>, max: u32) -> Result<String> {
			Ok(format!("messages:{system:?}:{max}"))
		}
	}

	/// system を先頭に足し、応答を `<>` で包む Hook。
	struct Wrap;

	#[async_trait::async_trait]
	impl Hook for Wrap {
		fn points(&self) -> Vec<HookPoint> {
			vec![HookPoint::BeforeRequest, HookPoint::AfterResponse]
		}
		async fn before_request(&self, mut req: ModelRequest) -> Result<ModelRequest> {
			req.messages.insert(0, ChatMessage::system("sys"));
			Ok(req)
		}
		async fn after_response(&self, mut c: Completion) -> Result<Completion> {
			c.text = format!("<{}>", c.text);
			Ok(c)
		}
	}

	#[derive(Default)]
	struct Rec {
		events: Mutex<Vec<&'static str>>,
		flushed: Mutex<usize>,
	}

	#[async_trait::async_trait]
	impl Observer for Rec {
		fn on_event(&self, event: &Event) {
			let kind = match event {
				Event::Request { .. } => "request",
				Event::Response { .. } => "response",
				Event::ToolResult { .. } => "tool_result",
			};
			self.events.lock().unwrap().push(kind);
		}
		async fn flush(&self) {
			*self.flushed.lock().unwrap() += 1;
		}
	}

	fn session(rec: &Arc<Rec>) -> Session {
		let mut hooks = HookRegistry::new().with(Arc::new(Wrap));
		hooks.observe(rec.clone());
		Session::new(Arc::new(Echo), ToolRegistry::new(), hooks)
	}

	async fn chat(s: &Session, route: Route, max_tokens: Option<u32>) -> Result<String> {
		let mut deltas = String::new();
		let out = s
			.chat(
				route,
				"m",
				vec![ChatMessage::user("hi")],
				max_tokens,
				&mut |d| deltas.push_str(d),
				&mut |_| {},
			)
			.await?;
		Ok(format!("{}|{deltas}", out.text))
	}

	#[tokio::test]
	async fn every_route_goes_through_the_hooks() {
		let rec = Arc::new(Rec::default());
		let s = session(&rec);
		assert_eq!(chat(&s, Route::Chat, None).await.unwrap(), "<2:hi>|");
		// ストリームの差分は Hook 前の本文
		assert_eq!(chat(&s, Route::Stream, None).await.unwrap(), "<2:hi>|2:hi");
		assert_eq!(chat(&s, Route::Responses, None).await.unwrap(), "<responses:sys\nhi>|");
		assert_eq!(
			chat(&s, Route::Messages, Some(9)).await.unwrap(),
			r#"<messages:Some("sys"):9>|"#
		);
		assert_eq!(rec.events.lock().unwrap().len(), 8);
		assert_eq!(*rec.flushed.lock().unwrap(), 4, "each call flushes the observers");
	}

	#[tokio::test]
	async fn messages_requires_max_tokens_and_still_flushes() {
		let rec = Arc::new(Rec::default());
		let err = chat(&session(&rec), Route::Messages, None).await.unwrap_err();
		assert!(matches!(err, Error::Config(_)));
		assert_eq!(*rec.flushed.lock().unwrap(), 1);
	}
}
