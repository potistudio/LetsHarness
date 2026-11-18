//! [`Provider::chat`](crate::Provider::chat) の入出力: wire に依らない依頼 ([`ChatRequest`]) と、
//! 応答の流れ ([`ChatStream`] = [`ChatEvent`] の列)。
//!
//! ストリームで返すかどうかは表示側の都合なので、Provider は常にストリームを返す。
//! 一括で欲しい側は [`collect`] で最後の [`ChatEvent::Done`] まで読む。

use std::future::Future;

use futures_util::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{ChatMessage, Completion, Error, Result, ToolSpec};

/// モデルへの1回分の依頼。どの wire (`chat/completions` / `messages` / ...) で送るかは Provider が決める。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
	/// `system` メッセージも含む。wire ごとの置き場所 (Anthropic のトップレベル `system` など) へは Provider が移す。
	pub messages: Vec<ChatMessage>,
	/// モデルが呼べるツール。空ならツールなし。
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub tools: Vec<ToolSpec>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_tokens: Option<u32>,
	/// Provider 固有の指定 (例: commandcode の `{"wire": "responses"}`)。kernel は中を見ず、知らない Provider は無視する。
	#[serde(default, skip_serializing_if = "Value::is_null")]
	pub extra: Value,
}

impl ChatRequest {
	pub fn new(messages: Vec<ChatMessage>) -> Self {
		Self {
			messages,
			..Self::default()
		}
	}
}

/// 応答の流れの1つ。差分 (`*_delta`) は0回以上、最後に必ず1回 [`Done`](ChatEvent::Done)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
	/// 本文の差分。
	TextDelta { text: String },
	/// 思考過程 (reasoning / extended thinking) の差分。
	ThinkingDelta { text: String },
	/// 応答の全体。差分をつないだものと同じ本文・思考過程に、ツール呼び出しと使用量が付く。
	Done { completion: Completion },
}

/// [`Provider::chat`](crate::Provider::chat) の戻り値。`Err` が来たらそこで終わる。
pub type ChatStream<'a> = BoxStream<'a, Result<ChatEvent>>;

/// [`chat_stream`] の中で差分を流す口。
#[derive(Clone)]
pub struct ChatSink(mpsc::UnboundedSender<ChatEvent>);

impl ChatSink {
	pub fn text(&self, text: &str) {
		if !text.is_empty() {
			let _ = self.0.send(ChatEvent::TextDelta { text: text.to_owned() });
		}
	}

	pub fn thinking(&self, text: &str) {
		if !text.is_empty() {
			let _ = self.0.send(ChatEvent::ThinkingDelta { text: text.to_owned() });
		}
	}
}

/// 差分を `sink` に流しながら最後に応答の全体を返す `async` 処理を、[`ChatStream`] にする。
///
/// Provider の実装はふつうこれで `chat` を書く。ストリームを捨てると処理も止まる (読まれた分しか進まない)。
pub fn chat_stream<'a, F, Fut>(f: F) -> ChatStream<'a>
where
	F: FnOnce(ChatSink) -> Fut,
	Fut: Future<Output = Result<Completion>> + Send + 'a,
{
	let (tx, rx) = mpsc::unbounded_channel();
	let state = State {
		work: Some(Box::pin(f(ChatSink(tx)))),
		rx,
		result: None,
	};
	stream::unfold(Some(state), |state| async move {
		let mut st = state?;
		loop {
			let Some(work) = st.work.as_mut() else {
				// 処理は終わった。残りの差分を出してから結果を出す。
				if let Ok(event) = st.rx.try_recv() {
					return Some((Ok(event), Some(st)));
				}
				let result = st.result.take().expect("set when the work finished");
				return Some((result.map(|completion| ChatEvent::Done { completion }), None));
			};
			let next = tokio::select! {
				biased;
				Some(event) = st.rx.recv() => Ok(event),
				result = work => Err(result),
			};
			match next {
				Ok(event) => return Some((Ok(event), Some(st))),
				Err(result) => {
					st.work = None;
					st.result = Some(result);
				}
			}
		}
	})
	.boxed()
}

struct State<'a> {
	work: Option<futures_util::future::BoxFuture<'a, Result<Completion>>>,
	rx: mpsc::UnboundedReceiver<ChatEvent>,
	result: Option<Result<Completion>>,
}

/// 差分を `on_text` / `on_thinking` に渡しながら読み切り、[`ChatEvent::Done`] の応答を返す。
pub async fn collect(
	mut stream: ChatStream<'_>,
	on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
	on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<Completion> {
	while let Some(event) = stream.next().await {
		match event? {
			ChatEvent::TextDelta { text } => on_text(&text),
			ChatEvent::ThinkingDelta { text } => on_thinking(&text),
			ChatEvent::Done { completion } => return Ok(completion),
		}
	}
	Err(Error::plugin("kernel", "応答が `done` の前に終わりました"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn texts(events: &[ChatEvent]) -> Vec<String> {
		events
			.iter()
			.map(|e| match e {
				ChatEvent::TextDelta { text } => format!("t:{text}"),
				ChatEvent::ThinkingDelta { text } => format!("k:{text}"),
				ChatEvent::Done { completion } => format!("done:{}", completion.text),
			})
			.collect()
	}

	#[tokio::test]
	async fn deltas_come_before_done_in_order() {
		let stream = chat_stream(|sink| async move {
			sink.thinking("hm");
			sink.text("a");
			tokio::task::yield_now().await;
			sink.text("");
			sink.text("b");
			Ok(Completion {
				text: "ab".into(),
				..Default::default()
			})
		});
		let events: Vec<ChatEvent> = stream.map(|e| e.unwrap()).collect().await;
		assert_eq!(texts(&events), ["k:hm", "t:a", "t:b", "done:ab"]);
	}

	#[tokio::test]
	async fn errors_end_the_stream_and_collect_needs_done() {
		let stream = chat_stream(|sink| async move {
			sink.text("partial");
			Err(Error::Config("boom".into()))
		});
		let mut seen = String::new();
		let err = collect(stream, &mut |t| seen.push_str(t), &mut |_| {})
			.await
			.unwrap_err();
		assert!(matches!(err, Error::Config(_)));
		assert_eq!(seen, "partial");

		let empty: ChatStream<'_> = stream::empty().boxed();
		assert!(collect(empty, &mut |_| {}, &mut |_| {}).await.is_err());
	}

	#[test]
	fn events_cross_the_wire() {
		let v = serde_json::to_value(ChatEvent::TextDelta { text: "x".into() }).unwrap();
		assert_eq!(v, serde_json::json!({ "type": "text_delta", "text": "x" }));
		let req: ChatRequest = serde_json::from_value(serde_json::json!({ "messages": [] })).unwrap();
		assert!(req.tools.is_empty() && req.extra.is_null());
	}
}
