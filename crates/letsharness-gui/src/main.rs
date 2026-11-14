//! LetsHarness の Client (GUI)。Claude 風のシンプルなチャット画面。
//!
//! CLI と同じく `Kernel::boot` で Kernel を動かし、返ってきた [`Host`] に Provider を頼むだけ。
//! 起動引数の `-o key=value` はそのまま Kernel とプロバイダに渡す。

mod style;

use std::sync::Arc;

use iced::futures::channel::mpsc;
use iced::keyboard::{self, key};
use iced::task::{self, Task};
use iced::widget::{button, center, column, container, markdown, pick_list, row, scrollable, space, text, text_editor};
use iced::{Center, Element, Fill, Font, Length, Size};
use letsharness_kernel::{BootConfig, ChatMessage, ChatOutput, Host, Kernel, Provider};

/// `messages` wire (Anthropic) で使う出力上限。
const MAX_TOKENS: u32 = 4096;
/// 会話欄の最大幅。
const COLUMN_WIDTH: f32 = 720.0;

fn main() -> iced::Result {
	iced::application(App::boot, App::update, App::view)
		.title(App::title)
		.theme(|_: &App| style::theme())
		.window_size(Size::new(1100.0, 760.0))
		.run()
}

/// `-o key=value` を集める (CLI と同じ形)。
fn parse_options() -> Vec<(String, String)> {
	let mut args = std::env::args().skip(1);
	let mut options = Vec::new();
	while let Some(arg) = args.next() {
		let value = match arg.as_str() {
			"-o" | "--option" => args.next(),
			_ => arg.strip_prefix("--option=").map(str::to_owned),
		};
		if let Some((k, v)) = value.as_deref().and_then(|v| v.split_once('=')) {
			options.push((k.to_owned(), v.to_owned()));
		}
	}
	options
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
	User,
	Assistant,
}

struct Entry {
	role: Role,
	text: String,
	markdown: markdown::Content,
	thinking: String,
	show_thinking: bool,
	error: Option<String>,
	stopped: bool,
}

impl Entry {
	fn new(role: Role, text: String) -> Self {
		Self {
			role,
			markdown: markdown::Content::parse(&text),
			text,
			thinking: String::new(),
			show_thinking: false,
			error: None,
			stopped: false,
		}
	}
}

#[derive(Default)]
struct Conversation {
	entries: Vec<Entry>,
}

impl Conversation {
	fn title(&self) -> String {
		let first = self.entries.iter().find(|e| e.role == Role::User);
		let Some(first) = first else {
			return "New chat".into();
		};
		let flat = first.text.split_whitespace().collect::<Vec<_>>().join(" ");
		match flat.char_indices().nth(28) {
			Some((i, _)) => format!("{}…", &flat[..i]),
			None => flat,
		}
	}

	/// モデルに渡す履歴。失敗した応答は含めない。
	fn history(&self) -> Vec<ChatMessage> {
		self.entries
			.iter()
			.filter(|e| e.error.is_none() && !e.text.is_empty())
			.map(|e| match e.role {
				Role::User => ChatMessage::user(&e.text),
				Role::Assistant => ChatMessage::assistant(&e.text),
			})
			.collect()
	}
}

/// 応答中のリクエスト。`id` は古いストリームの差分を捨てるため。
struct Running {
	id: u64,
	conversation: usize,
	handle: task::Handle,
}

enum Boot {
	Booting,
	Ready(Arc<Host>),
	Failed(String),
}

struct App {
	kernel: Boot,
	options: Vec<(String, String)>,
	providers: Vec<String>,
	provider: Option<String>,
	client: Option<Arc<dyn Provider>>,
	models: Vec<String>,
	model: Option<String>,
	/// プロバイダ・モデル周りの問題 (認証不足など)。
	notice: Option<String>,
	conversations: Vec<Conversation>,
	current: usize,
	composer: text_editor::Content,
	running: Option<Running>,
	next_id: u64,
}

#[derive(Debug, Clone)]
enum Delta {
	Text(String),
	Thinking(String),
}

#[derive(Clone)]
enum Message {
	Booted(Result<Arc<Host>, String>),
	ProviderSelected(String),
	ProviderReady(String, Result<(Arc<dyn Provider>, Vec<String>), String>),
	ModelSelected(String),
	Edit(text_editor::Action),
	Send,
	Stop,
	Delta(u64, Delta),
	Finished(u64, Result<ChatOutput, String>),
	NewChat,
	SelectChat(usize),
	ToggleThinking(usize),
	Link(markdown::Uri),
}

impl App {
	fn boot() -> (Self, Task<Message>) {
		let options = parse_options();
		let config = BootConfig::new(options.clone());
		let app = Self {
			kernel: Boot::Booting,
			options,
			providers: Vec::new(),
			provider: None,
			client: None,
			models: Vec::new(),
			model: None,
			notice: None,
			conversations: vec![Conversation::default()],
			current: 0,
			composer: text_editor::Content::new(),
			running: None,
			next_id: 0,
		};
		let boot = Task::perform(
			async move { Kernel::boot(config).await.map(Arc::new).map_err(|e| e.to_string()) },
			Message::Booted,
		);
		(app, boot)
	}

	fn title(&self) -> String {
		match self.conversations.get(self.current) {
			Some(c) if !c.entries.is_empty() => format!("{} — LetsHarness", c.title()),
			_ => "LetsHarness".into(),
		}
	}

	fn update(&mut self, message: Message) -> Task<Message> {
		match message {
			Message::Booted(Ok(host)) => {
				self.providers = host.provider_kinds().into_iter().map(str::to_owned).collect();
				self.kernel = Boot::Ready(host);
				let default = ["commandcode", "mock"]
					.into_iter()
					.find(|k| self.providers.iter().any(|p| p == k))
					.map(str::to_owned)
					.or_else(|| self.providers.first().cloned());
				if let Some(kind) = default {
					return self.select_provider(kind);
				}
				self.notice = Some("プロバイダが見つかりません (LETSHARNESS_PLUGIN_PATH を確認してください)".into());
			}
			Message::Booted(Err(e)) => self.kernel = Boot::Failed(e),
			Message::ProviderSelected(kind) => return self.select_provider(kind),
			Message::ProviderReady(kind, result) => {
				if self.provider.as_deref() != Some(kind.as_str()) {
					return Task::none();
				}
				match result {
					Ok((client, models)) => {
						self.client = Some(client);
						self.model = models.first().cloned();
						self.notice = models.is_empty().then(|| format!("{kind}: モデルがありません"));
						self.models = models;
					}
					Err(e) => self.notice = Some(format!("{kind}: {e}")),
				}
			}
			Message::ModelSelected(model) => self.model = Some(model),
			Message::Edit(action) => self.composer.perform(action),
			Message::Send => return self.send(),
			Message::Stop => {
				if let Some(running) = self.running.take() {
					running.handle.abort();
					if let Some(entry) = self.conversations[running.conversation].entries.last_mut() {
						entry.stopped = true;
					}
				}
			}
			Message::Delta(id, delta) => {
				if let Some(entry) = self.running_entry(id) {
					match delta {
						Delta::Text(t) => {
							entry.text.push_str(&t);
							entry.markdown.push_str(&t);
						}
						Delta::Thinking(t) => entry.thinking.push_str(&t),
					}
				}
			}
			Message::Finished(id, result) => {
				if let Some(entry) = self.running_entry(id) {
					match result {
						Ok(out) => {
							entry.text = out.text;
							entry.markdown = markdown::Content::parse(&entry.text);
							if entry.thinking.trim().is_empty() {
								entry.thinking = out.thinking.unwrap_or_default();
							}
						}
						Err(e) => entry.error = Some(e),
					}
					self.running = None;
				}
			}
			Message::NewChat => {
				// 空の会話があればそれを開く (無駄に増やさない)。
				self.current = match self.conversations.iter().position(|c| c.entries.is_empty()) {
					Some(i) => i,
					None => {
						self.conversations.push(Conversation::default());
						self.conversations.len() - 1
					}
				};
				return iced::widget::operation::focus_next();
			}
			Message::SelectChat(i) => self.current = i,
			Message::ToggleThinking(i) => {
				if let Some(entry) = self.conversations[self.current].entries.get_mut(i) {
					entry.show_thinking = !entry.show_thinking;
				}
			}
			Message::Link(url) => open_url(&url),
		}
		Task::none()
	}

	/// プロバイダを作り、モデル一覧を引く。
	fn select_provider(&mut self, kind: String) -> Task<Message> {
		let Boot::Ready(host) = &self.kernel else {
			return Task::none();
		};
		self.provider = Some(kind.clone());
		self.client = None;
		self.models.clear();
		self.model = None;
		self.notice = None;
		let host = host.clone();
		let cfg = options_cfg(&self.options);
		let request = kind.clone();
		Task::perform(
			async move {
				let client = host.provider(&request, &cfg).await.map_err(|e| e.to_string())?;
				let models = client.list_models().await.map_err(|e| e.to_string())?;
				Ok((client, models.into_iter().map(|m| m.id).collect()))
			},
			move |r| Message::ProviderReady(kind.clone(), r),
		)
	}

	fn running_entry(&mut self, id: u64) -> Option<&mut Entry> {
		let running = self.running.as_ref().filter(|r| r.id == id)?;
		self.conversations[running.conversation].entries.last_mut()
	}

	fn can_send(&self) -> bool {
		self.running.is_none()
			&& self.client.is_some()
			&& self.model.is_some()
			&& !self.composer.text().trim().is_empty()
	}

	fn send(&mut self) -> Task<Message> {
		if !self.can_send() {
			return Task::none();
		}
		let (Some(client), Some(model)) = (self.client.clone(), self.model.clone()) else {
			return Task::none();
		};
		let prompt = self.composer.text().trim().to_owned();
		self.composer = text_editor::Content::new();

		let conversation = &mut self.conversations[self.current];
		conversation.entries.push(Entry::new(Role::User, prompt));
		let history = conversation.history();
		conversation.entries.push(Entry::new(Role::Assistant, String::new()));

		let id = self.next_id;
		self.next_id += 1;
		let (task, handle) = respond(client, model, history, id).abortable();
		self.running = Some(Running {
			id,
			conversation: self.current,
			handle,
		});
		task
	}

	fn view(&self) -> Element<'_, Message> {
		let divider = container(space()).width(1).height(Fill).style(style::divider);
		row![self.sidebar(), divider, self.main()].into()
	}

	fn sidebar(&self) -> Element<'_, Message> {
		let brand = row![
			text("✻").size(22).color(style::ACCENT),
			text("LetsHarness").size(18).font(style::SERIF)
		]
		.spacing(8)
		.align_y(Center);

		let new_chat = button(
			row![text("+").size(16).color(style::ACCENT), text("New chat").size(14)]
				.spacing(10)
				.align_y(Center),
		)
		.on_press(Message::NewChat)
		.width(Fill)
		.padding([8, 10])
		.style(style::nav(false));

		let recents = self
			.conversations
			.iter()
			.enumerate()
			.rev()
			.filter(|(i, c)| !c.entries.is_empty() || *i == self.current)
			.map(|(i, c)| {
				button(text(c.title()).size(13).wrapping(text::Wrapping::None))
					.on_press(Message::SelectChat(i))
					.width(Fill)
					.padding([7, 10])
					.style(style::nav(i == self.current))
					.into()
			});

		container(
			column![
				container(brand).padding([6, 10]),
				new_chat,
				container(text("Recents").size(12).color(style::MUTED)).padding([0, 10]),
				scrollable(column(recents).spacing(2)).height(Fill),
			]
			.spacing(12),
		)
		.width(260)
		.height(Fill)
		.padding(12)
		.style(style::sidebar)
		.into()
	}

	fn main(&self) -> Element<'_, Message> {
		match &self.kernel {
			Boot::Booting => return center(text("Starting kernel…").color(style::MUTED)).into(),
			Boot::Failed(e) => {
				return center(text(format!("Kernel の起動に失敗しました: {e}")).color(style::DANGER)).into();
			}
			Boot::Ready(_) => {}
		}

		let conversation = &self.conversations[self.current];
		if conversation.entries.is_empty() {
			let greeting = row![
				text("✻").size(34).color(style::ACCENT),
				text("How can I help you today?").size(34).font(style::SERIF)
			]
			.spacing(12)
			.align_y(Center);
			return center(
				column![greeting, self.composer()]
					.spacing(28)
					.align_x(Center)
					.max_width(COLUMN_WIDTH),
			)
			.padding(24)
			.into();
		}

		let entries = conversation.entries.iter().enumerate().map(|(i, e)| self.entry(i, e));
		let messages = scrollable(
			container(column(entries).spacing(24).max_width(COLUMN_WIDTH))
				.center_x(Fill)
				.padding([32, 24]),
		)
		.anchor_bottom()
		.height(Fill);

		column![
			messages,
			container(container(self.composer()).max_width(COLUMN_WIDTH))
				.center_x(Fill)
				.padding(iced::Padding::new(16.0).top(0).left(24).right(24)),
		]
		.into()
	}

	fn entry<'a>(&'a self, i: usize, e: &'a Entry) -> Element<'a, Message> {
		if e.role == Role::User {
			let bubble = container(text(&e.text).size(15))
				.padding([10, 14])
				.max_width(COLUMN_WIDTH * 0.8)
				.style(style::user_bubble);
			return row![space::horizontal(), bubble].into();
		}

		let mut body = column![].spacing(10);
		if !e.thinking.trim().is_empty() {
			let arrow = if e.show_thinking { "▾" } else { "▸" };
			body = body.push(
				button(text(format!("{arrow} Thinking")).size(13))
					.on_press(Message::ToggleThinking(i))
					.padding([4, 8])
					.style(style::ghost),
			);
			if e.show_thinking {
				body = body.push(
					container(text(e.thinking.trim()).size(13))
						.padding([10, 14])
						.width(Fill)
						.style(style::thinking),
				);
			}
		}
		let streaming = self
			.running
			.as_ref()
			.is_some_and(|r| r.conversation == self.current && i + 1 == self.conversations[self.current].entries.len());
		if !e.text.is_empty() {
			body = body.push(
				markdown::view_with(e.markdown.items(), style::markdown_settings(), &style::MarkdownViewer)
					.map(Message::Link),
			);
		} else if streaming {
			body = body.push(text("✻").size(20).color(style::ACCENT));
		}
		if let Some(error) = &e.error {
			body = body.push(text(error).size(13).color(style::DANGER));
		}
		if e.stopped {
			body = body.push(text("Stopped").size(12).color(style::MUTED));
		}
		body.into()
	}

	fn composer(&self) -> Element<'_, Message> {
		let editor = text_editor(&self.composer)
			.placeholder("Reply to LetsHarness…")
			.on_action(Message::Edit)
			.key_binding(|press| {
				let enter = matches!(press.key, keyboard::Key::Named(key::Named::Enter));
				if enter && !press.modifiers.shift() {
					return Some(text_editor::Binding::Custom(Message::Send));
				}
				text_editor::Binding::from_key_press(press)
			})
			.size(15)
			.padding(4)
			.min_height(48)
			.max_height(240)
			.style(style::editor);

		let provider = pick_list(
			self.providers.as_slice(),
			self.provider.as_ref(),
			Message::ProviderSelected,
		)
		.text_size(13)
		.padding([4, 8])
		.style(style::picker);
		let model = pick_list(self.models.as_slice(), self.model.as_ref(), Message::ModelSelected)
			.placeholder(if self.provider.is_some() && self.client.is_none() {
				"Loading…"
			} else {
				"Model"
			})
			.text_size(13)
			.padding([4, 8])
			.style(style::picker);

		let action = if self.running.is_some() {
			button(center(text("■").size(12)))
				.on_press(Message::Stop)
				.style(style::round(style::TEXT, style::MUTED))
		} else {
			button(center(text("↑").size(18).font(Font::MONOSPACE)))
				.on_press_maybe(self.can_send().then_some(Message::Send))
				.style(style::round(style::ACCENT, style::ACCENT_HOVER))
		}
		.width(34)
		.height(34)
		.padding(0);

		let mut composer = column![
			editor,
			row![provider, model, space::horizontal(), action]
				.spacing(4)
				.align_y(Center),
		]
		.spacing(8);
		if let Some(notice) = &self.notice {
			composer = composer.push(text(notice).size(12).color(style::DANGER));
		}
		container(composer)
			.padding([12, 14])
			.width(Length::Fill)
			.style(style::composer)
			.into()
	}
}

/// リンクを既定のブラウザで開く。
fn open_url(url: &str) {
	let mut command = if cfg!(target_os = "macos") {
		std::process::Command::new("open")
	} else if cfg!(windows) {
		let mut c = std::process::Command::new("cmd");
		c.args(["/C", "start", ""]);
		c
	} else {
		std::process::Command::new("xdg-open")
	};
	let _ = command.arg(url).spawn();
}

/// `-o` をプロバイダの `cfg` (JSON オブジェクト) にする。
fn options_cfg(options: &[(String, String)]) -> serde_json::Value {
	serde_json::Value::Object(
		options
			.iter()
			.map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
			.collect(),
	)
}

/// 1ターン分の応答。差分は [`Message::Delta`]、最後に全文を [`Message::Finished`] で返す。
///
/// `claude-*` は CLI と同じく `messages` wire (非ストリーム)、それ以外は `chat/completions` のストリーム。
fn respond(client: Arc<dyn Provider>, model: String, history: Vec<ChatMessage>, id: u64) -> Task<Message> {
	let (tx, rx) = mpsc::unbounded();
	let work = async move {
		if model.starts_with("claude-") {
			return client.messages_with_usage(&model, history, None, MAX_TOKENS).await;
		}
		let thinking_tx = tx.clone();
		let mut on_delta = move |d: &str| {
			let _ = tx.unbounded_send(Delta::Text(d.to_owned()));
		};
		let mut on_thinking = move |t: &str| {
			let _ = thinking_tx.unbounded_send(Delta::Thinking(t.to_owned()));
		};
		client
			.chat_stream_with_usage(&model, history, &mut on_delta, &mut on_thinking)
			.await
	};
	Task::batch([
		Task::run(rx, move |d| Message::Delta(id, d)),
		Task::perform(work, move |r| Message::Finished(id, r.map_err(|e| e.to_string()))),
	])
}
