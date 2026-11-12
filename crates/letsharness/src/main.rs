use std::sync::Arc;

use clap::{Parser, Subcommand};

use letsharness_kernel::{self as llm, ChatMessage, Provider};
use letsharness_provider_commandcode::CommandCodeProvider;
use letsharness_provider_mock::MockClient;

#[derive(Parser)]
#[command(name = "letsharness", about = "Command Code Provider API をRustから直接叩く")]
struct Cli {
	/// 差し替えポイント: commandcode | mock
	#[arg(long, global = true, default_value = "commandcode")]
	provider: String,
	/// mock用: 応答prefix (provider=mock時のみ)
	#[arg(long, global = true, default_value = "")]
	mock_prefix: String,
	/// mock用: 指定すると全リクエストをこのメッセージで失敗させる
	#[arg(long, global = true, default_value = None)]
	mock_error: Option<String>,

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
		/// テキストではなく `{text, usage}` をJSONで出す
		#[arg(long, default_value_t = false)]
		json: bool,
	},
	/// GET /alpha/billing/credits (+ subscriptions / usage/summary) の正規化
	///
	/// `/provider/v1/*` に課金面は無いため、CLIと同じ `/alpha` 面を叩く。
	Quota {
		#[arg(long)]
		json: bool,
	},
}

/// トレイト経由のファクトリ。ここを差し替えるだけで実装が切り替わる。
fn make_client(provider: &str, mock_prefix: &str, mock_error: Option<String>) -> anyhow::Result<Arc<dyn Provider>> {
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
			let client = CommandCodeProvider::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
			Ok(Arc::new(client))
		}
		other => Err(anyhow::anyhow!("unknown provider: {other} (commandcode|mock)")),
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let cli = Cli::parse();
	let client = make_client(&cli.provider, &cli.mock_prefix, cli.mock_error)?;

	match cli.cmd {
		Cmd::Models { limit, json } => {
			// `dyn LlmClient` に対して呼ぶ = どの実装でも同じコード
			let models = client.list_models().await.map_err(|e| anyhow::anyhow!("{e}"))?;

			if json {
				let out: Vec<_> = match limit {
					Some(l) => models.iter().take(l).collect(),
					None => models.iter().collect(),
				};
				println!("{}", serde_json::to_string_pretty(&out)?);
			} else {
				let total = models.len();
				let shown = limit.map(|l| total.min(l)).unwrap_or(total);
				println!("[{}] {} models (showing {} of {})", client.name(), total, shown, total);
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
		} => {
			run_chat(
				&*client, model, prompt, system, max_tokens, stream, route, show_usage, json,
			)
			.await?;
		}
		Cmd::Quota { json } => {
			run_quota(&*client, json).await?;
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
) -> anyhow::Result<()> {
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
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		emit_output(out.text, out.usage, json_output, show_usage, false);
	} else if wire == "responses" && !stream {
		let input = msgs.iter().map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
		let out = client
			.responses_with_usage(&model, &input)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		emit_output(out.text, out.usage, json_output, show_usage, false);
	} else if stream {
		let mut stdout = std::io::stdout();
		let mut on_delta = |d: &str| {
			if !json_output {
				print!("{d}");
				let _ = stdout.flush();
			}
		};
		let out = client
			.chat_stream_with_usage(&model, msgs, &mut on_delta)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		emit_output(out.text, out.usage, json_output, show_usage, !json_output);
	} else {
		let out = client
			.chat_with_usage(&model, msgs, max_tokens)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		emit_output(out.text, out.usage, json_output, show_usage, false);
	}
	Ok(())
}

fn emit_output(text: String, usage: Option<llm::Usage>, json_output: bool, show_usage: bool, stream_done: bool) {
	if json_output {
		let v = serde_json::json!({"text": text, "usage": usage});
		println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
		return;
	}
	if stream_done {
		// stream時は本文を逐次出しているため最後に改行だけ足す。
		println!();
	} else {
		println!("{text}");
	}
	if show_usage {
		match usage {
			Some(u) => eprintln!(
				"usage: prompt={} completion={} total={}{} latency_ms={:?} request_id={:?}",
				u.prompt_tokens,
				u.completion_tokens,
				u.total_tokens,
				u.cached_tokens.map(|c| format!(" cached={c}")).unwrap_or_default(),
				u.latency_ms,
				u.request_id,
			),
			None => eprintln!("usage: (none)"),
		}
	}
}

async fn run_quota(client: &dyn Provider, json: bool) -> anyhow::Result<()> {
	let quota = client.quota().await.map_err(|e| anyhow::anyhow!("{e}"))?;
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
