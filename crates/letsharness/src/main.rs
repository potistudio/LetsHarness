mod agent;

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};

use letsharness_agent_antigravity::AntigravityAgent;
use letsharness_kernel::{self as llm, AgentBackend, ChatMessage, Provider};
use letsharness_provider_commandcode::CommandCodeProvider;
use letsharness_provider_gemini::GeminiProvider;
use letsharness_provider_mock::MockClient;

#[derive(Parser)]
#[command(name = "letsharness", about = "Command Code Provider API をRustから直接叩く")]
struct Cli {
	/// モデルの差し替えポイント: commandcode | gemini | mock
	#[arg(long, global = true, default_value = "commandcode")]
	provider: String,
	/// エージェントの担い手 (`agent` / `models` で有効)
	///
	/// builtin     : --provider のモデル + 同梱ツールで LetsHarness がループを回す
	/// antigravity : Antigravity CLI (`agy`) に丸ごと任せる (ツール・認証も agy 側)
	#[arg(long, global = true, default_value = "builtin")]
	backend: String,
	/// mock用: 応答prefix (provider=mock時のみ)
	#[arg(long, global = true, default_value = "")]
	mock_prefix: String,
	/// mock用: 指定すると全リクエストをこのメッセージで失敗させる
	#[arg(long, global = true, default_value = None)]
	mock_error: Option<String>,
	/// 読み込む `.env` ファイル。省略時はカレントから親へ遡って `.env` を探す
	///
	/// 既に設定済みの環境変数は上書きしない (環境変数 > .env)。
	#[arg(long, global = true, value_name = "PATH", conflicts_with = "no_env_file")]
	env_file: Option<std::path::PathBuf>,
	/// `.env` を読み込まない
	#[arg(long, global = true, default_value_t = false)]
	no_env_file: bool,

	#[command(subcommand)]
	cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
	/// GET /models 一覧
	Models {
		/// 指定なしは無制限
		#[arg(long)]
		limit: Option<usize>,

		#[arg(long)]
		json: bool,
	},

	/// POST /chat/completions (Claudeは自動で/messagesに切替)
	Chat {
		#[arg(long)]
		model: String,
		#[arg(long)]
		prompt: String,
		#[arg(long)]
		system: Option<String>,
		#[arg(long)]
		max_tokens: Option<u32>,
		#[arg(long, default_value_t = false)]
		stream: bool,
		/// auto | chat | responses | messages
		#[arg(long, default_value = "auto")]
		route: String,
		/// レスポンスの `usage` (prompt/completion tokens) をstderrに表示
		#[arg(long, default_value_t = false)]
		show_usage: bool,
		/// テキストではなく `{text, thinking, usage, stats}` をJSONで出す
		#[arg(long, default_value_t = false)]
		json: bool,
		/// 思考過程 (reasoning / extended thinking) を表示しない (既定は表示)
		#[arg(long, default_value_t = false)]
		no_thinking: bool,
	},
	/// GET /alpha/billing/credits (+ subscriptions / usage/summary) の正規化
	///
	/// `/provider/v1/*` に課金面は無いため、CLIと同じ `/alpha` 面を叩く。
	Quota {
		#[arg(long)]
		json: bool,
	},
	/// ツールを使いながら最終回答まで自走する (非ストリーム)
	///
	/// --backend builtin: 同梱ツール read_file / list_dir (承認不要)、write_file / edit_file / bash (承認が必要)。
	/// 承認は実行ごとに stderr で確認する。`--yes` で全承認 (bash は何でも実行できるので注意)。
	///
	/// --backend antigravity: agy が自分のツールで動く。承認はこちらに求められず、
	/// agy の権限設定で許可されていない操作は却下される (`--yes` で全許可)。
	Agent {
		/// builtin では必須。antigravity で省略すると agy の既定モデル
		#[arg(long)]
		model: Option<String>,
		#[arg(long)]
		prompt: String,
		#[arg(long)]
		system: Option<String>,
		#[arg(long)]
		max_tokens: Option<u32>,
		/// モデル呼び出しの上限回数 (暴走防止)
		#[arg(long, default_value_t = llm::DEFAULT_MAX_STEPS)]
		max_steps: usize,
		/// ツールの作業ディレクトリ。ファイル系ツールはこの外に出られない
		#[arg(long, default_value = ".")]
		workdir: std::path::PathBuf,
		/// 承認が必要なツールを確認なしで実行する
		#[arg(long, default_value_t = false)]
		yes: bool,
		/// 思考過程を表示しない
		#[arg(long, default_value_t = false)]
		no_thinking: bool,
		/// 全ステップ合算の usage をstderrに表示
		#[arg(long, default_value_t = false)]
		show_usage: bool,
		/// 外部エージェントの会話を続きから再開する (終了時に表示される session ID)
		#[arg(long, value_name = "SESSION_ID")]
		resume: Option<String>,
	},
}

/// トレイト経由のファクトリ。ここを差し替えるだけで実装が切り替わる。
fn make_client(provider: &str, mock_prefix: &str, mock_error: Option<String>) -> Result<Arc<dyn Provider>> {
	match provider {
		"mock" => {
			if let Some(msg) = mock_error {
				Ok(Arc::new(MockClient::with_error(msg)))
			} else if mock_prefix.is_empty() {
				Ok(Arc::new(MockClient::new()))
			} else {
				Ok(Arc::new(MockClient::with_prefix(mock_prefix)))
			}
		}
		"commandcode" => {
			let client = CommandCodeProvider::from_env()?;
			Ok(Arc::new(client))
		}
		"gemini" => {
			let client = GeminiProvider::from_env()?;
			Ok(Arc::new(client))
		}
		"antigravity" | "agy" => Err(anyhow!(
			"antigravity はモデルではなくエージェントです: `--backend antigravity agent ...` を使ってください"
		)),
		other => Err(anyhow!("unknown provider: {other} (commandcode|gemini|mock)")),
	}
}

/// 外部エージェントのファクトリ。`builtin` は `--provider` から組み立てるので `None`。
fn make_external(backend: &str, workdir: Option<&std::path::Path>) -> Result<Option<Arc<dyn AgentBackend>>> {
	match backend {
		"builtin" => Ok(None),
		// Antigravity CLI (`agy`) をサブプロセスで使う。認証は agy 側。
		"antigravity" | "agy" => {
			let mut agent = AntigravityAgent::from_env();
			if let Some(dir) = workdir {
				agent = agent.with_workdir(dir);
			}
			Ok(Some(Arc::new(agent)))
		}
		other => Err(anyhow!("unknown backend: {other} (builtin|antigravity)")),
	}
}

/// `chat` / `quota` は Provider (モデル) 専用。
fn ensure_builtin(backend: &str, command: &str) -> Result<()> {
	if backend == "builtin" {
		return Ok(());
	}
	bail!("`{command}` はモデル (--provider) 専用です。--backend は `agent` / `models` でのみ使えます")
}

fn main() -> Result<()> {
	let cli = Cli::parse();
	if !cli.no_env_file {
		// SAFETY: tokio ランタイム起動前で、環境変数に触るスレッドは main のみ。
		unsafe { letsharness_plugin_dotenv::load(cli.env_file.as_deref()) }?;
	}
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
	// 外部エージェントだけ使うときに Provider の認証を要求しないよう、必要になるまで作らない。
	let client = || make_client(&cli.provider, &cli.mock_prefix, cli.mock_error.clone());

	match cli.cmd {
		Cmd::Models { limit, json } => {
			// `dyn Provider` / `dyn AgentBackend` に対して呼ぶ = どの実装でも同じコード
			let (name, models) = match make_external(&cli.backend, None)? {
				Some(agent) => (agent.name(), agent.list_models().await),
				None => {
					let client = client()?;
					(client.name(), client.list_models().await)
				}
			};
			let models = models?;

			if json {
				let out: Vec<_> = match limit {
					Some(l) => models.iter().take(l).collect(),
					None => models.iter().collect(),
				};
				println!("{}", serde_json::to_string_pretty(&out)?);
			} else {
				let total = models.len();
				let shown = limit.map(|l| total.min(l)).unwrap_or(total);
				println!("[{name}] {total} models (showing {shown} of {total})");
				for m in models.iter().take(shown) {
					let eps = m
						.supported_endpoints
						.as_deref()
						.map(|e| e.join(","))
						.unwrap_or_else(|| "-".into());
					println!("- {} (ctx={}, endpoints={})", m.id, m.context_length, eps);
				}
				if shown < total {
					eprintln!(
						"(truncated: showing first {shown} of {total}; omit --limit or use --limit {total} for the full list)"
					);
				}
			}
		}
		Cmd::Chat {
			model,
			prompt,
			system,
			max_tokens,
			stream,
			route,
			show_usage,
			json,
			no_thinking,
		} => {
			ensure_builtin(&cli.backend, "chat")?;
			run_chat(
				&*client()?,
				model,
				prompt,
				system,
				max_tokens,
				stream,
				route,
				show_usage,
				json,
				no_thinking,
			)
			.await?;
		}
		Cmd::Quota { json } => {
			ensure_builtin(&cli.backend, "quota")?;
			run_quota(&*client()?, json).await?;
		}
		Cmd::Agent {
			model,
			prompt,
			system,
			max_tokens,
			max_steps,
			workdir,
			yes,
			no_thinking,
			show_usage,
			resume,
		} => {
			let backend: Arc<dyn AgentBackend> = match make_external(&cli.backend, Some(&workdir))? {
				Some(agent) => agent,
				None => {
					if model.is_none() {
						bail!("--backend builtin では --model が必要です");
					}
					let mut tools = llm::ToolRegistry::new();
					tools.extend(letsharness_tool_builtin::builtin_tools(workdir));
					Arc::new(llm::BuiltinAgent::new(client()?, tools))
				}
			};
			agent::run(
				backend,
				agent::Options {
					model: model.unwrap_or_default(),
					prompt,
					system,
					max_tokens,
					max_steps,
					yes,
					no_thinking,
					show_usage,
					resume,
				},
			)
			.await?;
		}
	}
	Ok(())
}

/// ハーネス本体の例: 具象型ではなく `&dyn LlmClient` / `impl LlmClient` に依存する.
/// この関数は commandcode / mock のどちらでも動く.
#[allow(clippy::too_many_arguments)]
async fn run_chat(
	client: &dyn Provider,
	model: String,
	prompt: String,
	system: Option<String>,
	max_tokens: Option<u32>,
	stream: bool,
	route: String,
	show_usage: bool,
	json_output: bool,
	no_thinking: bool,
) -> Result<()> {
	use std::io::Write;

	let mut msgs: Vec<ChatMessage> = Vec::new();
	if let Some(s) = system.clone() {
		msgs.push(ChatMessage::system(s));
	}
	msgs.push(ChatMessage::user(prompt));

	// ルーティング決定
	let wire = if route != "auto" {
		route.clone()
	} else if model.starts_with("claude-") {
		"messages".to_string()
	} else {
		// カタログがあればresponses対応を確認 (失敗時はchat)
		match client.list_models().await {
			Ok(models) => {
				let found = models.iter().find(|m| m.id == model);
				llm::route_for_model(&model, found.and_then(|m| m.supported_endpoints.as_deref())).to_string()
			}
			Err(_) => "chat/completions".to_string(),
		}
	};

	if wire == "messages" {
		let out = client
			.messages_with_usage(&model, msgs, system.as_deref(), max_tokens.unwrap_or(1024))
			.await?;
		emit_output(
			out.text,
			out.thinking,
			out.usage,
			json_output,
			show_usage,
			false,
			no_thinking,
		);
	} else if wire == "responses" && !stream {
		let input = msgs.iter().map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
		let out = client.responses_with_usage(&model, &input).await?;
		emit_output(
			out.text,
			out.thinking,
			out.usage,
			json_output,
			show_usage,
			false,
			no_thinking,
		);
	} else if stream {
		use std::sync::Arc;
		use std::sync::atomic::{AtomicBool, Ordering};
		let mut stdout = std::io::stdout();
		// Thinking は本文とは別に stderr へ逐次出す。JSON時・--no-thinking時は抑止。
		// 本文の最初のデルタが来たら思考ブロックを閉じる (thinking→text の順で読みやすく)。
		let thinking_open = Arc::new(AtomicBool::new(false));
		let thinking_closed = Arc::new(AtomicBool::new(false));
		let thinking_open_delta = Arc::clone(&thinking_open);
		let thinking_closed_delta = Arc::clone(&thinking_closed);
		let mut on_delta = |d: &str| {
			if json_output {
				return;
			}
			if !no_thinking
				&& thinking_open_delta.load(Ordering::SeqCst)
				&& !thinking_closed_delta.load(Ordering::SeqCst)
			{
				eprintln!("\n--- end of thinking ---");
				thinking_closed_delta.store(true, Ordering::SeqCst);
			}
			print!("{d}");
			let _ = stdout.flush();
		};
		let thinking_open_th = Arc::clone(&thinking_open);
		let thinking_closed_th = Arc::clone(&thinking_closed);
		let mut on_thinking = |t: &str| {
			if json_output || no_thinking {
				return;
			}
			// 本文開始後に思考が来る稀な形ではブロックを再開する。
			if thinking_closed_th.load(Ordering::SeqCst) {
				thinking_closed_th.store(false, Ordering::SeqCst);
			}
			if !thinking_open_th.load(Ordering::SeqCst) {
				eprintln!("--- thinking ---");
				thinking_open_th.store(true, Ordering::SeqCst);
			}
			eprint!("{t}");
			let _ = std::io::stderr().flush();
		};
		let out = client
			.chat_stream_with_usage(&model, msgs, &mut on_delta, &mut on_thinking)
			.await?;
		let header_printed = thinking_open.load(Ordering::SeqCst);
		let closed = thinking_closed.load(Ordering::SeqCst);
		if header_printed && !closed && !json_output && !no_thinking {
			eprintln!("\n--- end of thinking ---");
		}
		// 人間向け表示ではライブ表示済みのため再表示しない (Noneを渡す)。
		// JSON時は結果に含める必要があるためそのまま渡す。
		let thinking_for_output = if json_output {
			out.thinking
		} else if header_printed {
			None
		} else {
			out.thinking
		};
		emit_output(
			out.text,
			thinking_for_output,
			out.usage,
			json_output,
			show_usage,
			!json_output,
			no_thinking,
		);
	} else {
		let out = client.chat_with_usage(&model, msgs, max_tokens).await?;
		emit_output(
			out.text,
			out.thinking,
			out.usage,
			json_output,
			show_usage,
			false,
			no_thinking,
		);
	}
	Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_output(
	text: String,
	thinking: Option<String>,
	usage: Option<llm::Usage>,
	json_output: bool,
	show_usage: bool,
	stream_done: bool,
	no_thinking: bool,
) {
	// --no-thinking 時は思考過程を捨てる (JSON の thinking も null になる)。
	let thinking = if no_thinking { None } else { thinking };
	let thinking = thinking.filter(|t| !t.trim().is_empty());
	if json_output {
		let stats = usage.as_ref().map(llm::Usage::stats);
		let v = serde_json::json!({"text": text, "thinking": thinking, "usage": usage, "stats": stats});
		println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
		return;
	}
	// Thinking は本文より先に stderr へ。stdout は text のみでパイプ可能に保つ。
	if let Some(t) = thinking.as_deref() {
		eprintln!("--- thinking ---\n{t}\n--- end of thinking ---");
	}
	if stream_done {
		// stream時は本文を逐次出しているため最後に改行だけ足す。
		println!();
	} else {
		println!("{text}");
	}
	if show_usage {
		match usage {
			Some(u) => eprintln!("{}", format_usage(&u)),
			None => eprintln!("usage: (none)"),
		}
	}
}

/// `usage: in=.. (cached=.. NN.N%, uncached=..) out=.. total=.. tok/s=.. ...`
fn format_usage(u: &llm::Usage) -> String {
	let s = u.stats();
	let mut cache = match s.cache_hit_pct {
		Some(pct) => format!(
			" (cached={} {pct:.1}%, uncached={}",
			s.cached_tokens, s.uncached_input_tokens
		),
		None => String::new(),
	};
	if let Some(w) = u.cache_write_tokens {
		cache.push_str(if cache.is_empty() { " (" } else { ", " });
		cache.push_str(&format!("cache_write={w}"));
	}
	if !cache.is_empty() {
		cache.push(')');
	}
	let reasoning = u
		.reasoning_tokens
		.map(|r| format!(" (reasoning={r})"))
		.unwrap_or_default();
	let tps = s
		.output_tokens_per_sec
		.map(|t| format!("{t:.1}"))
		.unwrap_or_else(|| "-".into());
	let ttft = u.ttft_ms.map(|t| format!(" ttft_ms={t}")).unwrap_or_default();
	let latency = u.latency_ms.map(|l| l.to_string()).unwrap_or_else(|| "-".into());
	let request_id = u.request_id.as_deref().unwrap_or("-");
	format!(
		"usage: in={}{cache} out={}{reasoning} total={} tok/s={tps} latency_ms={latency}{ttft} request_id={request_id}",
		s.input_tokens, s.output_tokens, u.total_tokens,
	)
}

/// Calls `client.quota()` and prints the result in a human-readable format or JSON.
async fn run_quota(client: &dyn Provider, json: bool) -> Result<()> {
	let quota = client.quota().await?;

	if json {
		println!("{}", serde_json::to_string_pretty(&quota)?);
		return Ok(());
	}

	println!("[{}] quota", client.name());
	if let Some(plan) = &quota.plan {
		let status = quota.status.as_deref().unwrap_or("-");
		println!("plan: {plan} ({status})");
	}
	if let Some(b) = quota.credit_balance {
		println!("credits: remaining={b:.4}");
		if let Some(m) = quota.monthly_remaining {
			println!("  monthly_remaining={m:.4}");
		}
	}
	if let Some(w) = &quota.five_hour {
		print_window("5h", w);
	}
	if let Some(w) = &quota.weekly {
		print_window("weekly", w);
	}
	if let Some(w) = &quota.monthly {
		print_window("monthly", w);
	}
	if let Some(spend) = quota.period_spend {
		let tokens = quota.period_tokens.map(|t| t.to_string()).unwrap_or_else(|| "-".into());
		println!("period: spend={spend:.4} tokens={tokens}");
	}
	if quota.plan.is_none() && quota.credit_balance.is_none() && quota.five_hour.is_none() {
		eprintln!("(empty quota: --json で生値を確認)");
	}
	Ok(())
}

fn print_window(label: &str, w: &llm::QuotaWindow) {
	println!(
		"{label}: used={:.4} ({:.1}%) / cap={:.4} (remaining={:.4}, {:.1}%) reset_ms={:?} exceeded={}",
		w.used,
		w.used_pct,
		w.cap,
		w.remaining,
		w.remaining_pct(),
		w.reset_at_ms,
		w.exceeded
	);
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 差し替え可能性の証明: 同じジェネリック関数が両実装で動く
	async fn echo_hello(client: &impl Provider) -> String {
		client
			.chat("any-model", vec![ChatMessage::user("hello")], None)
			.await
			.unwrap_or_else(|e| e.to_string())
	}

	#[tokio::test]
	async fn mock_client_is_swappable() {
		let mock = MockClient::new();
		assert_eq!(mock.name(), "mock");
		assert_eq!(echo_hello(&mock).await, "[mock] any-model: hello");

		// カスタムprefixでも同じトレイトで動く
		let prefixed = MockClient::with_prefix(">> ");
		assert_eq!(echo_hello(&prefixed).await, ">> any-model: hello");

		// dyn 経由でも同じ
		let dyn_client: Arc<dyn Provider> = Arc::new(MockClient::new());
		let models = dyn_client.list_models().await.unwrap();
		assert!(!models.is_empty());
	}

	#[test]
	fn format_usage_line() {
		let u = llm::Usage {
			prompt_tokens: 200,
			completion_tokens: 50,
			total_tokens: 250,
			cached_tokens: Some(150),
			latency_ms: Some(1500),
			ttft_ms: Some(500),
			request_id: Some("r1".into()),
			..Default::default()
		};
		assert_eq!(
			format_usage(&u),
			"usage: in=200 (cached=150 75.0%, uncached=50) out=50 total=250 tok/s=50.0 latency_ms=1500 ttft_ms=500 request_id=r1"
		);
		let bare = llm::Usage {
			prompt_tokens: 3,
			completion_tokens: 3,
			total_tokens: 6,
			..Default::default()
		};
		assert_eq!(
			format_usage(&bare),
			"usage: in=3 out=3 total=6 tok/s=- latency_ms=- request_id=-"
		);
	}

	#[tokio::test]
	async fn mock_client_error_path() {
		let failing = MockClient::with_error("boom");
		let err = failing
			.chat("m", vec![ChatMessage::user("hi")], None)
			.await
			.unwrap_err()
			.to_string();
		assert!(err.contains("boom"), "unexpected: {err}");
	}
}
