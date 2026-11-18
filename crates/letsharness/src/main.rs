//! LetsHarness の Client (CLI)。
//!
//! やることは `Kernel::boot` で Kernel を動かし、返ってきた [`Host`] に頼むだけ。
//! どんなプラグインがあるか、どのプロセスでどう動いているかは知らない。

mod exec;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use letsharness_kernel::{
	self as llm, AgentBackend, BootConfig, ChatMessage, ChatRequest, Host, Kernel, Session, SessionId,
};

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
	// `-o` はそのまま Kernel に渡す。`boot` を持つプラグインとプロバイダが必要なものを拾う。
	let mut config = BootConfig::new(cli.options.clone());
	if let Some(profile) = &cli.profile {
		config.profile = Some(profile.clone());
	}
	config.patches = cli.patch.clone();
	if let Some(path) = &cli.config {
		if !path.is_file() {
			bail!("--config {}: ファイルがありません", path.display());
		}
		config.config_file = Some(path.clone());
	}
	// プラグインの管理はプラグインを起動しない。
	if let Cmd::Plugin { cmd } = &cli.cmd {
		return run_plugin(&config, cmd);
	}
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(async {
			let host = Kernel::boot(config).await?;
			run(&host, cli).await
		})
}

/// `plugin list` / `enable` / `disable`。切り替えは profile のパッチ (`patch.toml`) に書く。
fn run_plugin(config: &BootConfig, cmd: &PluginCmd) -> Result<()> {
	let plugins = Kernel::plugins(config)?;
	let (id, enabled) = match cmd {
		PluginCmd::List { json } => {
			print_plugins(config, &plugins, *json)?;
			return Ok(());
		}
		PluginCmd::Enable { id } => (id, true),
		PluginCmd::Disable { id } => (id, false),
	};
	if !plugins.iter().any(|p| &p.manifest.name == id) {
		let known: Vec<&str> = plugins.iter().map(|p| p.manifest.name.as_str()).collect();
		bail!("unknown plugin: {id} ({})", known.join("|"));
	}
	let Some(path) = config.profile_patch()? else {
		bail!("profile がありません (--profile を指定してください)");
	};
	let mut patch = llm::Patch::load_or_default(&path)?;
	patch.set_enabled(&path, id, enabled)?;
	patch.save(&path)?;

	let now = Kernel::plugins(config)?
		.into_iter()
		.find(|p| &p.manifest.name == id)
		.is_some_and(|p| p.enabled);
	println!(
		"{id}: {} ({})",
		if now { "enabled" } else { "disabled" },
		path.display()
	);
	if now != enabled {
		eprintln!("warning: --patch が profile の設定を上書きしています");
	}
	Ok(())
}

fn print_plugins(config: &BootConfig, plugins: &[llm::PluginState], json: bool) -> Result<()> {
	if json {
		let out: Vec<Value> = plugins
			.iter()
			.map(|p| {
				json!({
					"name": p.manifest.name,
					"enabled": p.enabled,
					"order": p.order,
					"contributes": p.manifest.contributes.summary(),
					"config": p.config,
					"dir": p.manifest.dir,
				})
			})
			.collect();
		println!("{}", serde_json::to_string_pretty(&out)?);
		return Ok(());
	}
	if let Some(path) = config.profile_patch()? {
		let profile = config.profile.as_deref().unwrap_or_default();
		println!("profile: {profile} ({})", path.display());
	}
	for patch in &config.patches {
		println!("patch: {}", patch.display());
	}
	for p in plugins {
		let mark = if p.enabled { "on " } else { "off" };
		let order = if p.order == 0 {
			String::new()
		} else {
			format!(" order={}", p.order)
		};
		let config = if p.config.is_empty() {
			String::new()
		} else {
			format!(" config={}", Value::Object(p.config.clone()))
		};
		println!(
			"{mark} {} [{}]{order}{config}",
			p.manifest.name,
			p.manifest.contributes.summary().join(", ")
		);
	}
	Ok(())
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
	/// モデルの差し替えポイント: 設定ファイルの `[providers.NAME]` か、プロバイダの kind (commandcode | gemini | mock)
	///
	/// 省略すると設定ファイルの `default_provider`、それも無ければ commandcode。
	#[arg(long, global = true)]
	provider: Option<String>,
	/// 外部エージェントの kind (`agent` で必須。`models` で指定すると、そのエージェントのモデルを引く)
	///
	/// antigravity : Antigravity CLI (`agy`) に丸ごと任せる (ツール・認証も agy 側)
	#[arg(long, global = true)]
	backend: Option<String>,
	/// プラグイン固有の設定 (例: `-o prefix=">> "` / `-o error=boom` は mock、`-o env_file=PATH` / `-o no_env_file=true` は dotenv)
	///
	/// パッチ層の `config` より強い。
	#[arg(short = 'o', long = "option", global = true, value_name = "KEY=VALUE", value_parser = parse_option)]
	options: Vec<(String, String)>,
	/// 使う profile (既定は $LETSHARNESS_PROFILE、無ければ default)。<home>/profiles/<NAME>/patch.toml を最初の層にする
	#[arg(long, global = true, value_name = "NAME")]
	profile: Option<String>,
	/// profile の後に重ねるパッチ (指定順。プラグインの有効/無効・並び順・設定を変える)
	#[arg(long, global = true, value_name = "FILE")]
	patch: Vec<std::path::PathBuf>,
	/// 設定ファイル (既定は $LETSHARNESS_CONFIG、無ければ <home>/config.toml)。`default_provider` と `[providers.NAME]`
	#[arg(long, global = true, value_name = "FILE")]
	config: Option<std::path::PathBuf>,

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

	/// モデルを1回呼ぶ (wire はプロバイダが選ぶ。commandcode は claude-* なら /messages、他は /chat/completions)
	Chat {
		#[arg(long)]
		model: String,
		#[arg(long)]
		prompt: String,
		#[arg(long)]
		system: Option<String>,
		#[arg(long)]
		max_tokens: Option<u32>,
		/// 本文が届くたびに表示する (届き方はプロバイダの wire 次第)
		#[arg(long, default_value_t = false)]
		stream: bool,
		/// wire の指定: auto | chat | responses | messages (プロバイダへ `extra.wire` として渡る。commandcode が使う)
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
		/// 前の会話を続きから再開する (終了時に表示される session ID)。プロバイダは会話のものを使う
		#[arg(long, value_name = "SESSION_ID")]
		resume: Option<String>,
	},
	/// GET /alpha/billing/credits (+ subscriptions / usage/summary) の正規化
	///
	/// `/provider/v1/*` に課金面は無いため、CLIと同じ `/alpha` 面を叩く。
	Quota {
		#[arg(long)]
		json: bool,
	},
	/// --provider のモデルとツールでループを回し、最終回答まで進める (非ストリーム)
	///
	/// ツールは read_file / list_dir / write_file / edit_file / bash を確認なしで実行する
	/// (bash は何でも実行できるので注意)。
	Loop {
		#[arg(long)]
		model: String,
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
		/// 前の会話を続きから再開する (終了時に表示される session ID)。プロバイダは会話のものを使う
		#[arg(long, value_name = "SESSION_ID")]
		resume: Option<String>,
	},
	/// 外部エージェント (--backend) に最終回答まで丸ごと任せる (非ストリーム)
	///
	/// 例: --backend antigravity は agy が自分のツールで動く。権限は agy 側の設定に従う (全許可も agy 側で設定する)。
	/// 権限の扱いはエージェントごとに異なり、LetsHarness からは切り替えない。
	Agent {
		/// 省略するとエージェントの既定モデル
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
	/// プラグインの一覧と有効/無効の切り替え (profile の patch.toml に書く。プラグインは起動しない)
	Plugin {
		#[command(subcommand)]
		cmd: PluginCmd,
	},
}

#[derive(Subcommand)]
enum PluginCmd {
	/// 層を重ねた結果のプラグイン (並び順。無効なものも含む)
	List {
		#[arg(long)]
		json: bool,
	},
	/// profile でプラグインを有効にする
	Enable { id: String },
	/// profile でプラグインを無効にする
	Disable { id: String },
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

/// kernel に Session (Provider + ツール + Hook + Observer) を頼む。モデルはこれ経由でしか使わない。
async fn make_session(host: &Host, provider: &str, cfg: &Value) -> Result<Session> {
	match host.session(provider, cfg).await {
		Ok(s) => Ok(s),
		Err(_) if host.has_agent_backend(provider) => Err(anyhow!(
			"{provider} はモデルではなくエージェントです: `--backend {provider} agent ...` を使ってください"
		)),
		Err(e) => Err(e.into()),
	}
}

/// 新しい会話の Session か、`resume` の会話の続き。`--system` は新しい会話でだけ使える (履歴の途中に挟まないため)。
async fn open_session(
	host: &Host,
	provider: &str,
	cfg: &Value,
	resume: Option<&str>,
	system: Option<&String>,
) -> Result<Session> {
	let Some(id) = resume else {
		return make_session(host, provider, cfg).await;
	};
	if system.is_some() {
		bail!("--system は新しい会話でだけ使えます (--resume とは併用できません)");
	}
	Ok(host.resume(&SessionId::parse(id)?, cfg).await?)
}

/// 会話を続けるための ID を stderr へ。
fn report_session(session: &Session) {
	if session.is_logged() {
		let id = session.id();
		eprintln!("session: {id} (--resume {id} で続きから)");
	}
}

/// 外部エージェントを kernel に頼む。`-o` はそのまま渡し、必要なエージェントだけが使う。
async fn make_agent(
	host: &Host,
	backend: &str,
	options: &[(String, String)],
	workdir: Option<&std::path::Path>,
) -> Result<Arc<dyn AgentBackend>> {
	let mut cfg = options_cfg(options);
	if let Some(dir) = workdir {
		cfg["workdir"] = json!(dir);
	}
	Ok(host.agent_backend(backend, &cfg).await?)
}

/// `chat` / `quota` / `loop` は Provider (モデル) を使う。外部エージェントとは別。
fn ensure_provider_only(backend: &Option<String>, command: &str) -> Result<()> {
	if backend.is_none() {
		return Ok(());
	}
	bail!("`{command}` はモデル (--provider) を使います。--backend は `agent` / `models` でのみ使えます")
}

async fn run(host: &Host, cli: Cli) -> Result<()> {
	let provider = cli
		.provider
		.clone()
		.or_else(|| host.default_provider().map(str::to_owned))
		.unwrap_or_else(|| "commandcode".into());
	let provider = provider.as_str();
	// 外部エージェントだけ使うときに Provider の認証を要求しないよう、必要になるまで作らない。
	let session = || async { make_session(host, provider, &options_cfg(&cli.options)).await };

	match cli.cmd {
		Cmd::Models { limit, json } => {
			// `dyn Provider` / `dyn AgentBackend` に対して呼ぶ = どの実装でも同じコード
			let (name, models) = match &cli.backend {
				Some(backend) => {
					let agent = make_agent(host, backend, &cli.options, None).await?;
					(agent.name(), agent.list_models().await)
				}
				None => {
					let session = session().await?;
					(session.provider_name(), session.list_models().await)
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
			resume,
		} => {
			ensure_provider_only(&cli.backend, "chat")?;
			let cfg = options_cfg(&cli.options);
			let session = open_session(host, provider, &cfg, resume.as_deref(), system.as_ref()).await?;
			run_chat(
				&session,
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
			report_session(&session);
		}
		Cmd::Quota { json } => {
			ensure_provider_only(&cli.backend, "quota")?;
			run_quota(&session().await?, json).await?;
		}
		Cmd::Loop {
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
			ensure_provider_only(&cli.backend, "loop")?;
			// ツールの作業ディレクトリは `workdir`、プロバイダ固有の設定は `-o`。
			let mut cfg = options_cfg(&cli.options);
			cfg["workdir"] = json!(workdir);
			exec::run_loop(
				&open_session(host, provider, &cfg, resume.as_deref(), system.as_ref()).await?,
				exec::Options {
					model,
					prompt,
					system,
					max_tokens,
					max_steps,
					no_thinking,
					show_usage,
					resume: None,
				},
			)
			.await?;
		}
		Cmd::Plugin { .. } => unreachable!("run_cli がプラグインを起動せずに処理する"),
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
			let Some(backend) = &cli.backend else {
				bail!(
					"agent には --backend が必要です (登録済み: {})",
					host.agent_backend_kinds().join("|")
				);
			};
			let backend = make_agent(host, backend, &cli.options, Some(&workdir)).await?;
			exec::run_agent(
				backend,
				exec::Options {
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

/// 1回のモデル呼び出し。どのプロバイダでも同じコードで、Hook と Observer は Session が効かせる。
#[allow(clippy::too_many_arguments)]
async fn run_chat(
	session: &Session,
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
	use std::sync::atomic::{AtomicBool, Ordering};

	let mut msgs: Vec<ChatMessage> = Vec::new();
	if let Some(s) = system {
		msgs.push(ChatMessage::system(s));
	}
	msgs.push(ChatMessage::user(prompt));

	let req = ChatRequest {
		max_tokens,
		extra: match route.as_str() {
			"auto" => Value::Null,
			wire => json!({ "wire": wire }),
		},
		..ChatRequest::new(msgs)
	};
	let live = stream && !json_output;

	// `--stream` 時のみ表示する。Thinking は本文とは別に stderr へ逐次出す。JSON時・--no-thinking時は抑止。
	// 本文の最初のデルタが来たら思考ブロックを閉じる (thinking→text の順で読みやすく)。
	let thinking_open = AtomicBool::new(false);
	let thinking_closed = AtomicBool::new(false);
	let mut stdout = std::io::stdout();
	let streamed = AtomicBool::new(false);
	let mut on_delta = |d: &str| {
		if !live {
			return;
		}
		streamed.store(true, Ordering::SeqCst);
		if !no_thinking && thinking_open.load(Ordering::SeqCst) && !thinking_closed.load(Ordering::SeqCst) {
			eprintln!("\n--- end of thinking ---");
			thinking_closed.store(true, Ordering::SeqCst);
		}
		print!("{d}");
		let _ = stdout.flush();
	};
	let mut on_thinking = |t: &str| {
		if !live || no_thinking {
			return;
		}
		// 本文開始後に思考が来る稀な形ではブロックを再開する。
		thinking_closed.store(false, Ordering::SeqCst);
		if !thinking_open.swap(true, Ordering::SeqCst) {
			eprintln!("--- thinking ---");
		}
		eprint!("{t}");
		let _ = std::io::stderr().flush();
	};
	let out = llm::collect(session.chat(&model, req), &mut on_delta, &mut on_thinking).await?;

	let streamed = streamed.load(Ordering::SeqCst);
	let header_printed = thinking_open.load(Ordering::SeqCst);
	if header_printed && !thinking_closed.load(Ordering::SeqCst) && !json_output && !no_thinking {
		eprintln!("\n--- end of thinking ---");
	}
	// 人間向け表示ではライブ表示済みの思考は再表示しない。JSON時は結果に含める。
	let thinking = if header_printed && !json_output {
		None
	} else {
		out.thinking
	};
	emit_output(
		out.text,
		thinking,
		out.usage,
		json_output,
		show_usage,
		streamed,
		no_thinking,
	);
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

/// Calls `session.quota()` and prints the result in a human-readable format or JSON.
async fn run_quota(session: &Session, json: bool) -> Result<()> {
	let quota = session.quota().await?;

	if json {
		println!("{}", serde_json::to_string_pretty(&quota)?);
		return Ok(());
	}

	println!("[{}] quota", session.provider_name());
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
