//! Antigravity CLI (`agy`) を外部エージェントとして使う [`AgentBackend`] 実装.
//!
//! Docs: https://antigravity.google/docs/cli/headless
//!
//! - 一覧 : `agy models`
//! - 実行 : `agy -p <prompt> --output-format stream-json --disable-slash-commands
//!   [--model <m>] [--conversation <id>]`
//!
//! agy はモデルではなくエージェント: ループ・ツール・会話履歴を自分で持つ。
//! そのため [`Provider`](letsharness_kernel::Provider) ではなく [`AgentBackend`] として載せる。
//! 認証・クォータも agy 側 (Google アカウント / `~/.gemini/antigravity-cli/`) に任せ、HTTP は叩かない。
//!
//! kernel の契約との対応:
//! - ツールは agy のもの。ハンドラには実行の通知 (`on_tool_call` / `on_tool_result`) だけが届く。
//!   権限は agy の設定に従う (未許可のものはヘッドレスで自動却下される)。
//!   全許可にしたいときは agy 側の権限設定で行う (LetsHarness からは切り替えない)。
//! - system プロンプト・履歴を渡す口が無いので1つのプロンプトに畳む ([`events::render_prompt`])。
//!   `session_id` 指定時は agy が履歴を持つので最後の依頼だけを送る。
//! - `max_tokens` / `max_steps` は指定できない (無視する)。
//! - usage は agy が内部で行った全モデル呼び出しの合算。

mod plugin;

pub use plugin::server;

mod events;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use letsharness_kernel::{
	AgentBackend, AgentHandler, AgentOutcome, AgentRequest, ChatMessage, Error, ModelInfo, Result,
};

use crate::events::{Event, RunResult, StepTracker};

const PLUGIN: &str = "antigravity";
const DEFAULT_BIN: &str = "agy";
const INSTALL_HINT: &str = "インストール: curl -fsSL https://antigravity.google/cli/install.sh | bash \
	(パスは AGY_BIN で指定可)";
const AUTH_HINT: &str =
	"agy の認証が必要です。一度 `agy` を対話モードで起動して Google アカウントでログインしてください";
/// エラーに添える stderr の最大文字数 (末尾を残す)。
const STDERR_TAIL_CHARS: usize = 2000;

// -----------------------------------------------------------------------------
// Config
// -----------------------------------------------------------------------------

/// `agy` の場所。優先順位: `AGY_BIN` > `PATH` 上の `agy` > `~/.local/bin/agy` (公式インストーラの既定)。
pub fn find_binary() -> PathBuf {
	if let Some(bin) = std::env::var_os("AGY_BIN").filter(|v| !v.is_empty()) {
		return bin.into();
	}
	let in_path = std::env::var_os("PATH").and_then(|paths| {
		std::env::split_paths(&paths)
			.map(|dir| dir.join(DEFAULT_BIN))
			.find(|p| p.is_file())
	});
	let local = || {
		std::env::var_os("HOME")
			.map(|h| PathBuf::from(h).join(".local/bin").join(DEFAULT_BIN))
			.filter(|p| p.is_file())
	};
	in_path.or_else(local).unwrap_or_else(|| DEFAULT_BIN.into())
}

/// `AGY_ARGS` (空白区切り) を毎回の起動に足す。
/// 例: `AGY_ARGS="--effort high"` / `AGY_ARGS="--mode plan"`。
fn args_from_env() -> Vec<String> {
	std::env::var("AGY_ARGS")
		.map(|s| s.split_whitespace().map(str::to_string).collect())
		.unwrap_or_default()
}

/// `--model` を付けないモデル名 (agy の既定モデルを使う)。
fn model_flag(model: &str) -> Option<&str> {
	let m = model.trim();
	(!(m.is_empty() || m.eq_ignore_ascii_case("default"))).then_some(m)
}

// -----------------------------------------------------------------------------
// AgentBackend Implementation
// -----------------------------------------------------------------------------

/// AgentBackend Implementation for Antigravity CLI (`agy`).
#[derive(Debug, Clone)]
pub struct AntigravityAgent {
	/// Path to the Antigravity CLI binary (`agy`).
	bin: PathBuf,

	/// The workspace directory for agy (the child process's current working directory).
	///
	/// If `None`, the parent's cwd is used.
	workdir: Option<PathBuf>,

	/// Additional arguments to pass to `agy` on every invocation.
	extra_args: Vec<String>,
}

impl AntigravityAgent {
	/// Create a new AntigravityAgent with the specified binary path.
	pub fn new(bin: impl Into<PathBuf>) -> Self {
		Self {
			bin: bin.into(),
			workdir: None,
			extra_args: Vec::new(),
		}
	}

	pub fn from_env() -> Self {
		Self::new(find_binary()).with_args(args_from_env())
	}

	pub fn with_workdir(mut self, dir: impl Into<PathBuf>) -> Self {
		self.workdir = Some(dir.into());
		self
	}

	/// 毎回の起動に足す追加引数 (`--effort high` など)。
	pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
		self.extra_args.extend(args.into_iter().map(Into::into));
		self
	}

	pub fn bin(&self) -> &Path {
		&self.bin
	}

	fn command(&self) -> tokio::process::Command {
		let mut cmd = tokio::process::Command::new(&self.bin);
		cmd.args(&self.extra_args)
			// stdin を開けたままだと待ち続ける実装があるため閉じる。
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			// 呼び出し側が future を捨てたら agy も止める。
			.kill_on_drop(true);
		if let Some(dir) = &self.workdir {
			cmd.current_dir(dir);
		}
		cmd
	}

	fn spawn_error(&self, e: std::io::Error) -> Error {
		if e.kind() == std::io::ErrorKind::NotFound {
			return Error::Config(format!("{} が見つかりません。{INSTALL_HINT}", self.bin.display()));
		}
		Error::plugin(PLUGIN, e)
	}
}

/// 失敗の要約 + stderr 末尾。認証切れは設定エラーとして案内する。
fn failure(summary: &str, stderr: &str) -> Error {
	if stderr.to_ascii_lowercase().contains("authentication required")
		|| summary.to_ascii_lowercase().contains("authentication required")
	{
		return Error::Config(AUTH_HINT.into());
	}
	let stderr = stderr.trim();
	if stderr.is_empty() {
		return Error::plugin(PLUGIN, summary.to_string());
	}
	let skip = stderr.chars().count().saturating_sub(STDERR_TAIL_CHARS);
	let tail: String = stderr.chars().skip(skip).collect();
	Error::plugin(PLUGIN, format!("{summary}\n--- agy stderr ---\n{tail}"))
}

#[async_trait::async_trait]
impl AgentBackend for AntigravityAgent {
	fn name(&self) -> &'static str {
		PLUGIN
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		let out = self
			.command()
			.arg("models")
			.output()
			.await
			.map_err(|e| self.spawn_error(e))?;
		if !out.status.success() {
			return Err(failure(
				&format!("agy models が失敗しました ({})", out.status),
				&String::from_utf8_lossy(&out.stderr),
			));
		}
		Ok(events::parse_models(&String::from_utf8_lossy(&out.stdout))
			.into_iter()
			.map(|id| ModelInfo {
				name: id.clone(),
				id,
				// agy はコンテキスト長を報告しない。
				context_length: 0,
				supported_endpoints: None,
			})
			.collect())
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn AgentHandler) -> Result<AgentOutcome> {
		let started = std::time::Instant::now();
		let prompt = match &req.session_id {
			Some(_) => events::resume_prompt(&req.messages)
				.ok_or_else(|| Error::Config("再開する依頼 (user メッセージ) がありません".into()))?
				.to_string(),
			None => events::render_prompt(&req.messages),
		};

		let mut cmd = self.command();
		if let Some(m) = model_flag(&req.model) {
			cmd.arg("--model").arg(m);
		}
		if let Some(id) = &req.session_id {
			cmd.arg("--conversation").arg(id);
		}
		cmd.args(["--output-format", "stream-json", "--disable-slash-commands", "-p"])
			.arg(prompt);
		let mut child = cmd.spawn().map_err(|e| self.spawn_error(e))?;

		// stderr は並行で読み切る (パイプが詰まって agy が止まらないように)。
		let mut stderr = child.stderr.take().expect("stderr is piped");
		let stderr_task = tokio::spawn(async move {
			let mut buf = String::new();
			let _ = stderr.read_to_string(&mut buf).await;
			buf
		});

		let stdout = child.stdout.take().expect("stdout is piped");
		let mut lines = BufReader::new(stdout).lines();
		let mut tracker = StepTracker::default();
		let mut result: Option<RunResult> = None;
		while let Some(line) = lines.next_line().await.map_err(|e| Error::plugin(PLUGIN, e))? {
			match events::parse_line(&line) {
				Some(Event::Step(step)) => tracker.on_step(step, handler),
				Some(Event::Result(r)) => result = Some(r),
				Some(Event::Other) | None => {}
			}
		}
		let (streamed_text, tool_steps) = tracker.finish(handler);
		let status = child.wait().await.map_err(|e| Error::plugin(PLUGIN, e))?;
		let stderr = stderr_task.await.unwrap_or_default();

		let Some(result) = result else {
			return Err(failure(
				&format!("agy が結果を返さずに終了しました ({status})"),
				&stderr,
			));
		};
		if !result.is_success() {
			let msg = result.error.as_deref().unwrap_or("(エラー詳細なし)");
			return Err(failure(&format!("status={}: {msg}", result.status), &stderr));
		}

		// 最終回答は result.response を正とし、無ければストリームから組み立てたもの。
		let text = if result.response.is_empty() {
			streamed_text
		} else {
			result.response
		};
		let usage = result.usage.map(|u| {
			let mut usage = u.to_usage();
			usage.latency_ms = Some(started.elapsed().as_millis() as u64);
			usage.request_id = result.conversation_id.clone();
			usage
		});
		let mut messages = req.messages;
		messages.push(ChatMessage::assistant(text.clone()));
		Ok(AgentOutcome {
			text,
			messages,
			// ツールの後には必ずモデル呼び出しが続く、とみなした推定値。
			steps: tool_steps + 1,
			usage,
			hit_step_limit: false,
			session_id: result.conversation_id,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_model_is_not_passed() {
		assert_eq!(model_flag("gemini-3.8-flash-high"), Some("gemini-3.8-flash-high"));
		assert_eq!(model_flag(""), None);
		assert_eq!(model_flag("Default"), None);
	}

	#[test]
	fn failure_maps_auth_and_keeps_stderr_tail() {
		assert!(matches!(
			failure("x", "Error: authentication required"),
			Error::Config(m) if m == AUTH_HINT
		));
		let e = failure("boom", &"a".repeat(STDERR_TAIL_CHARS + 10)).to_string();
		assert!(e.contains("boom\n--- agy stderr ---\n"), "{e}");
		assert!(e.ends_with(&"a".repeat(STDERR_TAIL_CHARS)), "{e}");
		assert_eq!(failure("boom", "  ").to_string(), "antigravity: boom");
	}
}
