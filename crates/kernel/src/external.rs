//! 外部プラグイン (別プロセス) の発見・起動・通信。
//!
//! プラグインは `plugin.toml` (manifest) で自分が何を提供するかを宣言し、
//! 使われるときに初めて起動する。落ちても Kernel は巻き込まれない。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, OnceCell, mpsc, oneshot};

use crate::rpc::{Frame, WireError};
use crate::{Error, Result};

pub const MANIFEST_FILE: &str = "plugin.toml";

// ---------------------------------------------------------------------------
// Manifest / 発見
// ---------------------------------------------------------------------------

/// `plugin.toml`。
///
/// ```toml
/// name = "mock"
/// command = "letsharness-provider-mock"
/// [contributes]
/// provider = ["mock"]
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
	pub name: String,
	/// 実行ファイル。相対名は manifest のあるディレクトリ → Kernel の実行ファイルと同じ場所 → `PATH` の順に探す。
	pub command: String,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default)]
	pub contributes: Contributes,
	/// manifest のあるディレクトリ (読み込み時に設定)。
	#[serde(skip)]
	pub dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Contributes {
	/// 起動時に `boot` を呼ぶ (環境変数の供給など)。
	#[serde(default)]
	pub boot: bool,
	/// `Provider` の kind。
	#[serde(default)]
	pub provider: Vec<String>,
	/// `AgentBackend` の kind (別名もここに並べる)。
	#[serde(default)]
	pub agent: Vec<String>,
	/// `Tool` の供給元の名前。
	#[serde(default)]
	pub tool: Vec<String>,
}

impl Manifest {
	pub fn load(path: &Path) -> Result<Self> {
		let src = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
		let mut m: Manifest = toml::from_str(&src).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
		m.dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
		Ok(m)
	}

	fn resolve_command(&self) -> PathBuf {
		let c = Path::new(&self.command);
		if c.is_absolute() || c.components().count() > 1 {
			return self.dir.join(c);
		}
		let exe_dir = std::env::current_exe()
			.ok()
			.and_then(|p| p.parent().map(Path::to_path_buf));
		for dir in std::iter::once(self.dir.clone()).chain(exe_dir) {
			let candidate = dir.join(c);
			if candidate.is_file() {
				return candidate;
			}
		}
		c.to_path_buf()
	}
}

/// 既定の探索先: `$LETSHARNESS_PLUGIN_PATH` → 実行ファイル横の `plugins/` → `~/.letsharness/plugins`。
pub fn default_plugin_path() -> Vec<PathBuf> {
	let mut dirs: Vec<PathBuf> = std::env::var_os("LETSHARNESS_PLUGIN_PATH")
		.map(|v| std::env::split_paths(&v).collect())
		.unwrap_or_default();
	if let Some(dir) = std::env::current_exe()
		.ok()
		.and_then(|p| p.parent().map(|d| d.join("plugins")))
	{
		dirs.push(dir);
	}
	if let Some(home) = std::env::var_os("HOME") {
		dirs.push(PathBuf::from(home).join(".letsharness/plugins"));
	}
	dirs
}

/// `dirs` の直下 (`<dir>/*/plugin.toml`) から manifest を集める。壊れたものは警告して飛ばす。
/// 同じ `name` は先に見つかったものを使う。
pub fn discover(dirs: &[PathBuf]) -> Vec<Manifest> {
	let mut found: Vec<Manifest> = Vec::new();
	for dir in dirs {
		let Ok(read) = std::fs::read_dir(dir) else { continue };
		let mut paths: Vec<_> = read
			.filter_map(|e| e.ok())
			.map(|e| e.path().join(MANIFEST_FILE))
			.collect();
		paths.sort();
		for path in paths.into_iter().filter(|p| p.is_file()) {
			match Manifest::load(&path) {
				Ok(m) if found.iter().any(|f| f.name == m.name) => {
					eprintln!(
						"warning: plugin `{}` is duplicated ({}); ignored",
						m.name,
						path.display()
					);
				}
				Ok(m) => found.push(m),
				Err(e) => eprintln!("warning: skipping plugin: {e}"),
			}
		}
	}
	found
}

// ---------------------------------------------------------------------------
// 接続
// ---------------------------------------------------------------------------

/// 通信中にプラグインから届く通知 / 問い合わせ。
#[derive(Debug)]
pub struct Event {
	pub method: String,
	pub params: Value,
	/// 問い合わせ (応答が要る) のときの id。
	/// 現在 `PluginServer` にはこれを送る API が無い (プロトコル上の予約)。
	pub reply_id: Option<u64>,
}

/// [`Call`] の途中で届く [`Event`] の受け手。戻り値は問い合わせ (応答が要るもの) への答え。
#[async_trait::async_trait]
pub trait EventSink: Send {
	async fn on_event(&mut self, method: &str, params: Value) -> Option<Result<Value>>;
}

/// 途中経過を使わない呼び出し用。
pub struct NoEvents;

#[async_trait::async_trait]
impl EventSink for NoEvents {
	async fn on_event(&mut self, _method: &str, _params: Value) -> Option<Result<Value>> {
		None
	}
}

struct Pending {
	done: oneshot::Sender<std::result::Result<Value, WireError>>,
	events: mpsc::UnboundedSender<Event>,
}

#[derive(Default)]
struct Inner {
	pending: HashMap<u64, Pending>,
	/// 接続が閉じた理由。
	closed: Option<String>,
}

pub struct Connection {
	name: &'static str,
	stdin: Mutex<ChildStdin>,
	state: Arc<StdMutex<Inner>>,
	next_id: AtomicU64,
	_child: Child,
}

impl Connection {
	pub async fn spawn(manifest: &Manifest, name: &'static str, env: &HashMap<String, String>) -> Result<Arc<Self>> {
		let program = manifest.resolve_command();
		let mut child = Command::new(&program)
			.args(&manifest.args)
			.envs(env)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.kill_on_drop(true)
			.spawn()
			.map_err(|e| Error::plugin(name, format!("{}: {e}", program.display())))?;
		let stdin = child.stdin.take().expect("piped");
		let stdout = child.stdout.take().expect("piped");

		let state = Arc::new(StdMutex::new(Inner::default()));
		tokio::spawn(read_loop(BufReader::new(stdout), state.clone()));

		Ok(Arc::new(Self {
			name,
			stdin: Mutex::new(stdin),
			state,
			next_id: AtomicU64::new(1),
			_child: child,
		}))
	}

	pub fn name(&self) -> &'static str {
		self.name
	}

	async fn send(&self, frame: Frame) -> Result<()> {
		let mut line = serde_json::to_string(&frame).map_err(|e| Error::plugin(self.name, e))?;
		line.push('\n');
		let mut stdin = self.stdin.lock().await;
		stdin
			.write_all(line.as_bytes())
			.await
			.map_err(|e| Error::plugin(self.name, format!("write: {e}")))?;
		stdin
			.flush()
			.await
			.map_err(|e| Error::plugin(self.name, format!("write: {e}")))
	}

	/// リクエストを送り、応答を待つ [`Call`] を返す。`params` は JSON オブジェクト。
	pub async fn start(self: &Arc<Self>, method: &str, mut params: Value) -> Result<Call> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let (done_tx, done_rx) = oneshot::channel();
		let (events_tx, events_rx) = mpsc::unbounded_channel();
		{
			let mut inner = self.state.lock().unwrap();
			if let Some(reason) = &inner.closed {
				return Err(Error::plugin(self.name, reason.clone()));
			}
			inner.pending.insert(
				id,
				Pending {
					done: done_tx,
					events: events_tx,
				},
			);
		}
		let call = Call {
			conn: self.clone(),
			id,
			done: done_rx,
			events: events_rx,
		};
		if let Some(obj) = params.as_object_mut() {
			obj.insert("req".into(), id.into());
		}
		self.send(Frame::request(id, method, params)).await?;
		Ok(call)
	}

	/// 途中経過のない呼び出し。
	pub async fn call(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
		self.start(method, params).await?.drive(&mut NoEvents).await
	}
}

async fn read_loop(stdout: BufReader<tokio::process::ChildStdout>, state: Arc<StdMutex<Inner>>) {
	let mut lines = stdout.lines();
	let reason = loop {
		let line = match lines.next_line().await {
			Ok(Some(line)) => line,
			Ok(None) => break "plugin exited".to_string(),
			Err(e) => break format!("read: {e}"),
		};
		let Ok(frame) = serde_json::from_str::<Frame>(&line) else {
			eprintln!("warning: plugin sent a malformed line: {line}");
			continue;
		};
		let mut inner = state.lock().unwrap();
		match (frame.id, frame.method.clone()) {
			// 応答
			(Some(id), None) => {
				if let Some(p) = inner.pending.remove(&id) {
					let result = match frame.error {
						Some(e) => Err(e),
						None => Ok(frame.result.unwrap_or(Value::Null)),
					};
					let _ = p.done.send(result);
				}
			}
			// 通知 / プラグインからの問い合わせ
			(reply_id, Some(method)) => {
				if let Some(p) = frame.req().and_then(|r| inner.pending.get(&r)) {
					let _ = p.events.send(Event {
						method,
						params: frame.params.unwrap_or(Value::Null),
						reply_id,
					});
				}
			}
			(None, None) => {}
		}
	};
	let mut inner = state.lock().unwrap();
	inner.closed = Some(reason.clone());
	for (_, p) in inner.pending.drain() {
		let _ = p.done.send(Err(WireError {
			kind: "plugin".into(),
			message: reason.clone(),
			status: None,
			code: None,
		}));
	}
}

/// 進行中のリクエスト。
pub struct Call {
	conn: Arc<Connection>,
	id: u64,
	done: oneshot::Receiver<std::result::Result<Value, WireError>>,
	events: mpsc::UnboundedReceiver<Event>,
}

type Done = std::result::Result<std::result::Result<Value, WireError>, oneshot::error::RecvError>;

impl Call {
	/// 応答が来るまで、途中経過を `sink` に渡しながら待つ。
	pub async fn drive(mut self, sink: &mut dyn EventSink) -> Result<Value> {
		loop {
			tokio::select! {
				biased;
				ev = self.events.recv() => match ev {
					Some(ev) => self.handle(ev, sink).await,
					None => {
						let done = (&mut self.done).await;
						return self.finish(done);
					}
				},
				done = &mut self.done => {
					while let Ok(ev) = self.events.try_recv() {
						self.handle(ev, sink).await;
					}
					return self.finish(done);
				}
			}
		}
	}

	async fn handle(&self, ev: Event, sink: &mut dyn EventSink) {
		let answer = sink.on_event(&ev.method, ev.params).await;
		if let Some(id) = ev.reply_id {
			let reply = match answer {
				Some(Ok(v)) => Frame::ok(id, v),
				Some(Err(e)) => Frame::err(id, WireError::from(&e)),
				None => Frame::ok(id, Value::Null),
			};
			let _ = self.conn.send(reply).await;
		}
	}

	fn finish(&self, done: Done) -> Result<Value> {
		match done {
			Ok(Ok(v)) => Ok(v),
			Ok(Err(e)) => Err(e.into_error(self.conn.name)),
			Err(_) => Err(Error::plugin(self.conn.name, "connection closed")),
		}
	}
}

impl Drop for Call {
	fn drop(&mut self) {
		if let Ok(mut inner) = self.conn.state.lock() {
			inner.pending.remove(&self.id);
		}
	}
}

// ---------------------------------------------------------------------------
// 遅延起動する1プラグイン
// ---------------------------------------------------------------------------

pub struct PluginEntry {
	pub manifest: Manifest,
	/// エラー表示用 (`Error::Plugin` が `&'static str` を要求するため、起動時に1度だけ確保する)。
	pub name: &'static str,
	env: Arc<HashMap<String, String>>,
	conn: OnceCell<Arc<Connection>>,
}

impl PluginEntry {
	pub fn new(manifest: Manifest, env: Arc<HashMap<String, String>>) -> Self {
		let name: &'static str = Box::leak(manifest.name.clone().into_boxed_str());
		Self {
			manifest,
			name,
			env,
			conn: OnceCell::new(),
		}
	}

	/// 初回に起動して、以降は同じプロセスを使う。
	pub async fn connection(&self) -> Result<Arc<Connection>> {
		self.conn
			.get_or_try_init(|| Connection::spawn(&self.manifest, self.name, &self.env))
			.await
			.cloned()
	}
}
