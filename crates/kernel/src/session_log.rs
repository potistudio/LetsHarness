//! セッションログ: 会話を追記だけの JSON Lines に残し、そこから会話を復元する。
//!
//! 1行目は `{"type":"session", ...}` の見出しで、以降は次の2種類が起きた順に並ぶ。
//! - `{"type":"event", "event":"request" | "response" | "tool_result", ...}`:
//!   Hook をすべて通した後にモデル・ツールとやりとりした値 ([`Event`])。モデルに何が見えたかの記録。
//! - `{"type":"message", "message": {...}}`: 会話の履歴に加わったメッセージ。ターンが成功したときにまとめて書く。
//!
//! 復元 ([`read`]) は `message` だけを読む。失敗・中断したターンは `event` だけが残り、履歴には入らない。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{ChatMessage, Error, Event, Observer, Result};

/// ログの形式の版。読めない版のログは復元しない。
pub const LOG_VERSION: u32 = 1;

/// セッションの ID。ログのファイル名にもなるので、英数字と `-` だけ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
	/// 新しい ID (`<epoch ms>-<pid>-<連番>`)。
	pub fn generate() -> Self {
		static NEXT: AtomicU64 = AtomicU64::new(0);
		let ms = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis())
			.unwrap_or_default();
		let n = NEXT.fetch_add(1, Ordering::Relaxed);
		Self(format!("{ms}-{}-{n}", std::process::id()))
	}

	pub fn parse(s: &str) -> Result<Self> {
		if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
			return Err(Error::Config(format!("session id `{s}`: 英数字と `-` だけが使えます")));
		}
		Ok(Self(s.to_owned()))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl std::fmt::Display for SessionId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.0)
	}
}

/// ログの1行。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
	Session { version: u32, id: String, provider: String },
	Message { message: ChatMessage },
	Event(Event),
}

/// `dir` 内のログの場所。
pub fn log_path(dir: &Path, id: &SessionId) -> PathBuf {
	dir.join(format!("{id}.jsonl"))
}

/// 1つのセッションのログ。最初に書くときにファイルを開く (使われなかったセッションはファイルを作らない)。
pub(crate) struct SessionLog {
	path: PathBuf,
	header: Record,
	file: Mutex<Option<File>>,
}

impl SessionLog {
	pub(crate) fn new(dir: &Path, id: &SessionId, provider: &str) -> Self {
		Self {
			path: log_path(dir, id),
			header: Record::Session {
				version: LOG_VERSION,
				id: id.to_string(),
				provider: provider.to_owned(),
			},
			file: Mutex::new(None),
		}
	}

	fn write(&self, record: &Record) -> Result<()> {
		let err = |e: std::io::Error| Error::Config(format!("session log {}: {e}", self.path.display()));
		let mut file = self.file.lock().unwrap();
		if file.is_none() {
			if let Some(dir) = self.path.parent() {
				std::fs::create_dir_all(dir).map_err(err)?;
			}
			let new = !self.path.exists();
			let mut opened = OpenOptions::new()
				.create(true)
				.append(true)
				.open(&self.path)
				.map_err(err)?;
			if new {
				writeln!(opened, "{}", line(&self.header)?).map_err(err)?;
			}
			*file = Some(opened);
		}
		let file = file.as_mut().expect("opened above");
		writeln!(file, "{}", line(record)?).map_err(err)
	}

	/// 履歴に加わったメッセージを書く。
	pub(crate) fn append_messages(&self, messages: &[ChatMessage]) -> Result<()> {
		for message in messages {
			self.write(&Record::Message {
				message: message.clone(),
			})?;
		}
		Ok(())
	}
}

fn line(record: &Record) -> Result<String> {
	serde_json::to_string(record).map_err(|e| Error::plugin("kernel", e))
}

/// やりとりは見るだけなので、書けなくてもターンは止めずに警告する。
#[async_trait::async_trait]
impl Observer for SessionLog {
	fn on_event(&self, event: &Event) {
		if let Err(e) = self.write(&Record::Event(event.clone())) {
			eprintln!("warning: {e}");
		}
	}
}

/// ログから `(provider の kind, 会話の履歴)` を読む。
///
/// 見出しが無い・版が違う・壊れた行がある場合は `Err(Config)`。ただし改行で終わっていない最後の行は、
/// 書いている途中で止まったものとして警告して捨てる。
pub(crate) fn read(path: &Path, id: &SessionId) -> Result<(String, Vec<ChatMessage>)> {
	let src =
		std::fs::read_to_string(path).map_err(|e| Error::Config(format!("session {id}: {}: {e}", path.display())))?;
	let at = |n: usize, msg: String| Error::Config(format!("{}:{n}: {msg}", path.display()));
	let lines: Vec<&str> = src.lines().collect();
	let mut provider = None;
	let mut history = Vec::new();
	for (i, text) in lines.iter().enumerate() {
		let record = match serde_json::from_str::<Record>(text) {
			Ok(record) => record,
			Err(_) if i + 1 == lines.len() && !src.ends_with('\n') => {
				eprintln!("warning: {}:{}: 書きかけの行を無視します", path.display(), i + 1);
				break;
			}
			Err(e) => return Err(at(i + 1, e.to_string())),
		};
		match (record, &provider) {
			(
				Record::Session {
					version,
					id: logged,
					provider: kind,
				},
				None,
			) => {
				if version != LOG_VERSION {
					return Err(at(
						i + 1,
						format!("version {version} は読めません (対応: {LOG_VERSION})"),
					));
				}
				if logged != id.as_str() {
					return Err(at(i + 1, format!("id `{logged}` が `{id}` と違います")));
				}
				provider = Some(kind);
			}
			(_, None) => return Err(at(i + 1, "見出し (`session`) がありません".into())),
			(Record::Session { .. }, Some(_)) => return Err(at(i + 1, "見出しが2つあります".into())),
			(Record::Message { message }, Some(_)) => history.push(message),
			(Record::Event(_), Some(_)) => {}
		}
	}
	let provider = provider.ok_or_else(|| at(1, "空のログです".into()))?;
	Ok((provider, history))
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;
	use crate::{ToolCall, ToolOutput};

	fn dir(name: &str) -> PathBuf {
		std::env::temp_dir().join(format!("lh-session-log-{name}-{}", std::process::id()))
	}

	#[test]
	fn messages_round_trip_and_events_are_skipped() {
		let dir = dir("rt");
		let id = SessionId::generate();
		let log = SessionLog::new(&dir, &id, "mock");
		assert!(!log_path(&dir, &id).exists(), "nothing is written until used");
		log.on_event(&Event::ToolResult {
			call: ToolCall::new("c1", "echo", json!({})),
			output: ToolOutput::ok("x"),
		});
		log.append_messages(&[ChatMessage::user("hi"), ChatMessage::assistant("yo")])
			.unwrap();

		let text = std::fs::read_to_string(log_path(&dir, &id)).unwrap();
		let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
		assert_eq!(first["type"], "session");
		let event: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
		assert_eq!(
			(event["type"].as_str(), event["event"].as_str()),
			(Some("event"), Some("tool_result"))
		);

		let (provider, history) = read(&log_path(&dir, &id), &id).unwrap();
		assert_eq!(provider, "mock");
		assert_eq!(
			history.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
			["hi", "yo"]
		);

		// 書きかけの最後の行は捨てる。途中の壊れた行は誤り。
		std::fs::write(log_path(&dir, &id), format!("{text}{{\"type\":\"mess")).unwrap();
		assert_eq!(read(&log_path(&dir, &id), &id).unwrap().1.len(), 2);
		std::fs::write(log_path(&dir, &id), format!("{text}oops\n")).unwrap();
		assert!(read(&log_path(&dir, &id), &id).is_err());
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn ids_cannot_escape_the_directory() {
		assert!(SessionId::parse("../x").is_err());
		assert!(SessionId::parse("").is_err());
		let id = SessionId::generate();
		assert_eq!(SessionId::parse(id.as_str()).unwrap(), id);
		assert_ne!(SessionId::generate(), SessionId::generate());
	}
}
