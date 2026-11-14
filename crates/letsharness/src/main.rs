//! LetsHarness の Client (CLI)。
//!
//! やることは `Kernel::boot` で Kernel を動かし、返ってきた [`Host`] に頼むだけ。
//! どんなプラグインがあるか、どのプロセスでどう動いているかは知らない。

mod agent;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use letsharness_kernel::{self as llm, AgentBackend, BootConfig, ChatMessage, Host, Kernel, Provider};

fn main() -> ExitCode {
	match run_cli() {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			eprintln!("Error: {e:?}");
			ExitCode::FAILURE
		}
	}
}

fn run_cli() -> Result<()> {
	let cli = Cli::parse();
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(async {
			// `-o` はそのまま Kernel に渡す。`boot` を持つプラグインとプロバイダが必要なものを拾う。
			let host = Kernel::boot(BootConfig::new(cli.options.clone())).await?;
			run(&host, cli).await
		})
}

/// `-o key=value`。
fn parse_option(s: &str) -> std::result::Result<(String, String), String> {
	s.split_once('=')
		.map(|(k, v)| (k.to_owned(), v.to_owned()))
		.ok_or_else(|| format!("`KEY=VALUE` の形式で指定してください: {s}"))
}

#[derive(Parser)]
#[command(name = "letsharness", about = "Command Code Provider API をRustから直接叩く")]
struct Cli {
	/// モデルの差し替えポイント (登録されたプロバイダの kind): commandcode | gemini | mock
	#[arg(long, global = true, default_value = "commandcode")]
	provider: String,
	/// エージェントの担い手 (`agent` / `models` で有効)
	///
	/// builtin     : --provider のモデル + 同梱ツールで LetsHarness がループを回す
	/// antigravity : Antigravity CLI (`agy`) に丸ごと任せる (ツール・認証も agy 側)
	#[arg(long, global = true, default_value = "builtin")]
	backend: String,
	/// プラグイン固有の設定 (例: `-o prefix=">> "` / `-o error=boom` は mock、`-o env_file=PATH` / `-o no_env_file=true` は dotenv)
	#[arg(short = 'o', long = "option", global = true, value_name = "KEY=VALUE", value_parser = parse_option)]
	options: Vec<(String, String)>,

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
	/// --backend builtin: 同梱ツール read_file / list_dir / write_file / edit_file / bash を確認なしで実行する
	/// (bash は何でも実行できるので注意)。
	///
	/// --backend antigravity: agy が自分のツールで動く。権限は agy の設定に従う (全許可も agy 側で設定する)。
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

/// `-o key=value` を `cfg` (JSON オブジェクト) にする。
fn options_cfg(options: &[(String, String)]) -> Value {
	Value::Object(
		options
			.iter()
			.map(|(k, v)| (k.clone(), Value::String(v.clone())))
			.collect(),
	)
}

/// kernel に Provider を頼む。
async fn make_client(host: &Host, provider: &str, options: &[(String, String)]) -> Result<Arc<dyn Provider>> {
	match host.provider(provider, &options_cfg(options)).await {
		Ok(p) => Ok(p),
		Err(_) if host.has_agent_backend(provider) => Err(anyhow!(
			"{provider} はモデルではなくエージェントです: `--backend {provider} agent ...` を使ってください"
		)),
		Err(e) => Err(e.into()),
	}
}

/// 外部エージェントを kernel に頼む。`builtin` は `--provider` から組み立てるので `None`。
async fn make_external(
	host: &Host,
	backend: &str,
	workdir: Option<&std::path::Path>,
) -> Result<Option<Arc<dyn AgentBackend>>> {
	if backend == "builtin" {
		return Ok(None);
	}
	let cfg = match workdir {
		Some(dir) => json!({ "workdir": dir }),
		None => json!({}),
	};
	Ok(Some(host.agent_backend(backend, &cfg).await?))
}

/// `chat` / `quota` は Provider (モデル) 専用。
fn ensure_builtin(backend: &str, command: &str) -> Result<()> {
	if backend == "builtin" {
		return Ok(());
	}
	bail!("`{command}` はモデル (--provider) 専用です。--backend は `agent` / `models` でのみ使えます")
}

async fn run(host: &Host, cli: Cli) -> Result<()> {
	// 外部エージェントだけ使うときに Provider の認証を要求しないよう、必要になるまで作らない。
	let client = || make_client(host, &cli.provider, &cli.options);

	match cli.cmd {
		Cmd::Models { limit, json } => {
			// `dyn Provider` / `dyn AgentBackend` に対して呼ぶ = どの実装でも同じコード
			let (name, models) = match make_external(host, &cli.backend, None).await? {
				Some(agent) => (agent.name(), agent.list_models().await),
				None => {
					let client = client().await?;
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
				&*client().await?,
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
			run_quota(&*client().await?, json).await?;
		}
		Cmd::Agent {
			model,
			prompt,
			system,
			max_tokens,
			max_steps,
			workdir,
			no_thinking,
			show_usage,
			resume,
		} => {
			let backend: Arc<dyn AgentBackend> = match make_external(host, &cli.backend, Some(&workdir)).await? {
				Some(agent) => agent,
				None => {
					if model.is_none() {
						bail!("--backend builtin では --model が必要です");
					}
					let tools = host.tools(&json!({ "workdir": workdir })).await?;
					Arc::new(llm::BuiltinAgent::new(client().await?, tools))
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
}
