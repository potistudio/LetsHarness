//! Session: Client がモデルを使うときの窓口で、1つの会話。
//!
//! Provider・ツール・Hook・Observer をひとまとめに持ち、1回のチャットにもループにも同じ Hook を効かせる。
//! 会話の履歴も Session が持ち、Client は毎ターンの新しいメッセージだけを渡す。
//! 保存先があれば会話をセッションログ (`<home>/sessions/<id>.jsonl`) に残し、[`Host::resume`](crate::Host::resume) で続きから再開できる。
//! Client は [`Host::session`](crate::Host::session) で作り、Provider を直接は触らない
//! (どの Client から使っても Hook を素通りさせないため)。

use std::path::PathBuf;
use std::sync::Arc;

use crate::session_log::{SessionId, SessionLog};
use crate::{
	ChatMessage, ChatRequest, ChatSink, ChatStream, Completion, Error, HookRegistry, LoopOutcome, LoopRequest,
	ModelInfo, ModelRequest, Provider, Quota, Result, RunHandler, ToolRegistry, chat_stream, collect, run_loop,
};

/// 会話をまたいで共有する部品 (プラグインとのやりとりなしに [`Session::fork`] できるように)。
struct Core {
	/// `--provider` の名前 (設定ファイルのインスタンス名か kind)。ログに残り、再開に使う。
	provider_ref: String,
	provider: Arc<dyn Provider>,
	tools: ToolRegistry,
	hooks: HookRegistry,
	/// セッションログの保存先。`None` なら残さない。
	log_dir: Option<PathBuf>,
}

struct Conversation {
	history: Vec<ChatMessage>,
	/// `history` のうちログに書き終えた数。
	logged: usize,
}

pub struct Session {
	core: Arc<Core>,
	id: SessionId,
	log: Option<Arc<SessionLog>>,
	/// ターンの間ずっと持つ。同じ Session のターンは1つずつ進む。
	conversation: tokio::sync::Mutex<Conversation>,
}

impl std::fmt::Debug for Session {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Session")
			.field("id", &self.id)
			.field("provider", &self.core.provider_ref)
			.finish_non_exhaustive()
	}
}

impl Session {
	/// 部品から作る (in-process で使うとき・テスト用)。ログは残さない。通常は [`Host::session`](crate::Host::session)。
	pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry, hooks: HookRegistry) -> Self {
		let core = Core {
			provider_ref: provider.name().to_owned(),
			provider,
			tools,
			hooks,
			log_dir: None,
		};
		Self::start(Arc::new(core), SessionId::generate(), Vec::new(), 0)
	}

	/// `provider_ref` (`--provider` の名前) の `provider` で、`log_dir` にログを残す新しい会話。
	pub(crate) fn create(
		provider_ref: &str,
		provider: Arc<dyn Provider>,
		tools: ToolRegistry,
		hooks: HookRegistry,
		log_dir: Option<PathBuf>,
	) -> Self {
		let core = Core {
			provider_ref: provider_ref.to_owned(),
			provider,
			tools,
			hooks,
			log_dir,
		};
		Self::start(Arc::new(core), SessionId::generate(), Vec::new(), 0)
	}

	/// ログから読んだ `history` の続きとして、同じ `id` で再開する。
	pub(crate) fn resumed(
		provider: Arc<dyn Provider>,
		provider_ref: &str,
		tools: ToolRegistry,
		hooks: HookRegistry,
		log_dir: PathBuf,
		id: SessionId,
		history: Vec<ChatMessage>,
	) -> Self {
		let core = Core {
			provider_ref: provider_ref.to_owned(),
			provider,
			tools,
			hooks,
			log_dir: Some(log_dir),
		};
		let logged = history.len();
		Self::start(Arc::new(core), id, history, logged)
	}

	fn start(core: Arc<Core>, id: SessionId, history: Vec<ChatMessage>, logged: usize) -> Self {
		let log = core
			.log_dir
			.as_ref()
			.map(|dir| Arc::new(SessionLog::new(dir, &id, &core.provider_ref)));
		Self {
			core,
			id,
			log,
			conversation: tokio::sync::Mutex::new(Conversation { history, logged }),
		}
	}

	/// 同じ Provider・ツール・Hook で、`history` から始まる新しい会話 (新しい ID) を作る。プラグインとはやりとりしない。
	///
	/// `history` は最初のターンが成功したときにログに書く。
	pub fn fork(&self, history: Vec<ChatMessage>) -> Session {
		Self::start(self.core.clone(), SessionId::generate(), history, 0)
	}

	pub fn id(&self) -> &SessionId {
		&self.id
	}

	/// セッションログに残るか (残るなら [`Host::resume`](crate::Host::resume) で再開できる)。
	pub fn is_logged(&self) -> bool {
		self.log.is_some()
	}

	/// `--provider` の名前 (設定ファイルのインスタンス名か kind)。
	pub fn provider(&self) -> &str {
		&self.core.provider_ref
	}

	/// Provider の名前。
	pub fn provider_name(&self) -> &'static str {
		self.core.provider.name()
	}

	/// ここまでの会話。
	pub async fn history(&self) -> Vec<ChatMessage> {
		self.conversation.lock().await.history.clone()
	}

	pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		self.core.provider.list_models().await
	}

	pub async fn quota(&self) -> Result<Quota> {
		self.core.provider.quota().await
	}

	/// このターンで使う Hook (共有の Hook・Observer に、この会話のログを足したもの)。
	fn hooks(&self) -> HookRegistry {
		let mut hooks = self.core.hooks.clone();
		if let Some(log) = &self.log {
			hooks.observe(log.clone());
		}
		hooks
	}

	/// 成功したターンを履歴として確定し、まだ書いていない分をログに書く。
	///
	/// ログに書けなくても会話は続け、警告する (書けなかった分は次のターンで書き直す)。
	fn commit(&self, conversation: &mut Conversation, history: Vec<ChatMessage>) {
		conversation.history = history;
		let Some(log) = &self.log else { return };
		match log.append_messages(&conversation.history[conversation.logged..]) {
			Ok(()) => conversation.logged = conversation.history.len(),
			Err(e) => eprintln!("warning: {e}"),
		}
	}

	/// ツールを使わない1回のモデル呼び出し。`req.messages` (このターンの新しいメッセージ) を履歴に続けて送る。
	///
	/// `before_request` / `after_response` の Hook を通し、Observer に知らせる。差分 (`*_delta`) は Hook を通す前の
	/// 本文で、最後の [`ChatEvent::Done`](crate::ChatEvent::Done) が Hook を通した応答。成功したときだけ
	/// `req.messages` と応答が履歴に加わる (失敗したターンは残らない)。ストリームを途中で捨てるとターンはそこで止まり、
	/// 終わっていなければ履歴に残らない。
	///
	/// ツールは使わない: `req.tools` が空でなければ `Err(Config)`、Hook が足したツールも送らない
	/// (ツールを使うなら [`run_loop`](Self::run_loop))。
	pub fn chat<'a>(&'a self, model: &'a str, req: ChatRequest) -> ChatStream<'a> {
		chat_stream(move |sink| async move {
			if !req.tools.is_empty() {
				return Err(Error::Config(
					"Session::chat はツールを使いません (ツールは run_loop で使います)".into(),
				));
			}
			let mut conversation = self.conversation.lock().await;
			let mut history = conversation.history.clone();
			history.extend(req.messages);
			let hooks = self.hooks();
			let request = ChatRequest {
				messages: history.clone(),
				tools: Vec::new(),
				max_tokens: req.max_tokens,
				extra: req.extra,
			};
			let out = self.chat_once(&hooks, ModelRequest::new(model, request), &sink).await;
			if let Ok(out) = &out {
				history.push(ChatMessage::assistant(out.text.clone()));
				self.commit(&mut conversation, history);
			}
			hooks.flush().await;
			out
		})
	}

	async fn chat_once(&self, hooks: &HookRegistry, req: ModelRequest, sink: &ChatSink) -> Result<Completion> {
		let (model, mut req) = hooks.before_request(req).await?.into_parts();
		req.tools.clear();
		let stream = self.core.provider.chat(&model, req);
		let completion = collect(stream, &mut |t| sink.text(t), &mut |t| sink.thinking(t)).await?;
		let mut completion = hooks.after_response(completion).await?;
		completion.tool_calls.clear();
		Ok(completion)
	}

	/// ツールを使いながら最終回答までループを回す ([`run_loop`])。`req.messages` はこのターンの新しいメッセージで、
	/// 履歴に続けて送る。成功したとき (`max_steps` での打ち切りも含む) は途中のツール往復ごと履歴に加わる。
	pub async fn run_loop(&self, mut req: LoopRequest, handler: &mut dyn RunHandler) -> Result<LoopOutcome> {
		let mut conversation = self.conversation.lock().await;
		let mut messages = conversation.history.clone();
		messages.append(&mut req.messages);
		req.messages = messages;
		let hooks = self.hooks();
		let out = run_loop(&*self.core.provider, &self.core.tools, &hooks, handler, req).await;
		if let Ok(out) = &out {
			self.commit(&mut conversation, out.messages.clone());
		}
		hooks.flush().await;
		out
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;
	use crate::{Event, Hook, HookPoint, Observer};

	/// 受け取ったメッセージ数と最後の本文を返す Provider。本文は1文字ずつ差分で流し、`extra` があれば添える。
	struct Echo;

	#[async_trait::async_trait]
	impl Provider for Echo {
		fn name(&self) -> &'static str {
			"echo"
		}
		async fn list_models(&self) -> Result<Vec<ModelInfo>> {
			Ok(vec![])
		}
		fn chat<'a>(&'a self, _: &'a str, req: ChatRequest) -> ChatStream<'a> {
			chat_stream(move |sink| async move {
				let mut text = format!("{}:{}", req.messages.len(), req.messages.last().unwrap().content);
				if !req.extra.is_null() {
					text.push_str(&format!(":{}", req.extra));
				}
				if let Some(max) = req.max_tokens {
					text.push_str(&format!(":{max}"));
				}
				for c in text.chars() {
					sink.text(&c.to_string());
				}
				Ok(Completion {
					text,
					..Default::default()
				})
			})
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

	/// `(応答, 差分をつないだもの)`。
	async fn chat_with(s: &Session, req: ChatRequest) -> Result<(String, String)> {
		let mut deltas = String::new();
		let out = collect(s.chat("m", req), &mut |d| deltas.push_str(d), &mut |_| {}).await?;
		Ok((out.text, deltas))
	}

	async fn chat(s: &Session) -> Result<String> {
		Ok(chat_with(s, ChatRequest::new(vec![ChatMessage::user("hi")])).await?.0)
	}

	#[tokio::test]
	async fn chat_goes_through_the_hooks() {
		let rec = Arc::new(Rec::default());
		let s = session(&rec);
		// 差分は Hook 前の本文、最後の応答は Hook 後
		assert_eq!(
			chat_with(&s, ChatRequest::new(vec![ChatMessage::user("hi")]))
				.await
				.unwrap(),
			("<2:hi>".into(), "2:hi".into())
		);
		let req = ChatRequest {
			max_tokens: Some(9),
			extra: serde_json::json!({ "wire": "x" }),
			..ChatRequest::new(vec![ChatMessage::user("hi")])
		};
		assert_eq!(
			chat_with(&s.fork(vec![]), req).await.unwrap().0,
			r#"<2:hi:{"wire":"x"}:9>"#,
			"max_tokens and extra reach the provider"
		);
		assert_eq!(
			*rec.events.lock().unwrap(),
			["request", "response", "request", "response"]
		);
		assert_eq!(*rec.flushed.lock().unwrap(), 2, "each call flushes the observers");
	}

	#[tokio::test]
	async fn chat_refuses_tools() {
		let rec = Arc::new(Rec::default());
		let req = ChatRequest {
			tools: vec![spec()],
			..ChatRequest::new(vec![ChatMessage::user("hi")])
		};
		let s = session(&rec);
		assert!(matches!(chat_with(&s, req).await, Err(Error::Config(_))));
		assert!(s.history().await.is_empty());
	}

	fn spec() -> crate::ToolSpec {
		crate::ToolSpec {
			name: "t".into(),
			description: String::new(),
			input_schema: serde_json::json!({}),
		}
	}

	fn contents(messages: &[ChatMessage]) -> Vec<String> {
		messages.iter().map(|m| format!("{}:{}", m.role, m.content)).collect()
	}

	#[tokio::test]
	async fn history_grows_only_with_successful_turns() {
		let s = session(&Arc::default());
		assert_eq!(chat(&s).await.unwrap(), "<2:hi>");
		// 2回目は前のターンを含む (sys は Hook がリクエストにだけ足すので履歴には無い)
		assert_eq!(chat(&s).await.unwrap(), "<4:hi>");
		assert!(
			chat_with(
				&s,
				ChatRequest {
					tools: vec![spec()],
					..ChatRequest::new(vec![ChatMessage::user("x")])
				}
			)
			.await
			.is_err()
		);
		assert_eq!(
			contents(&s.history().await),
			["user:hi", "assistant:<2:hi>", "user:hi", "assistant:<4:hi>"]
		);
		// fork は履歴を引き継いだ別の会話
		let forked = s.fork(s.history().await);
		assert_ne!(forked.id(), s.id());
		assert_eq!(chat(&forked).await.unwrap(), "<6:hi>");
		assert_eq!(s.history().await.len(), 4);
	}

	#[tokio::test]
	async fn the_log_restores_the_conversation() {
		let dir = std::env::temp_dir().join(format!("lh-session-{}", std::process::id()));
		let s = Session::create(
			"echo",
			Arc::new(Echo),
			ToolRegistry::new(),
			HookRegistry::new().with(Arc::new(Wrap)),
			Some(dir.clone()),
		);
		let seeded = s.fork(vec![ChatMessage::user("old"), ChatMessage::assistant("reply")]);
		chat(&seeded).await.unwrap();
		let path = crate::log_path(&dir, seeded.id());
		let (provider, history) = crate::session_log::read(&path, seeded.id()).unwrap();
		assert_eq!(provider, "echo");
		assert_eq!(history.len(), 4, "seeded history is written with the first turn");
		assert_eq!(contents(&history), contents(&seeded.history().await));
		// モデルに見えたもの (Hook が足した sys を含む) は event として残る
		let log = std::fs::read_to_string(&path).unwrap();
		assert!(log.contains(r#""type":"event","event":"request""#) && log.contains(r#""content":"sys""#));
		// 使われなかった会話はファイルを作らない
		assert!(!crate::log_path(&dir, s.id()).exists());
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
