mod commandcode;
mod llm;
mod mock;

use std::sync::Arc;

use clap::{Parser, Subcommand};

use commandcode::CommandCodeClient;
use llm::{ChatMessage, LlmClient};
use mock::MockClient;

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
		#[arg(long, default_value_t = 30)]
		limit: usize,
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
	},
}

/// トレイト経由のファクトリ。ここを差し替えるだけで実装が切り替わる。
fn make_client(provider: &str, mock_prefix: &str, mock_error: Option<String>) -> anyhow::Result<Arc<dyn LlmClient>> {
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
			let client = CommandCodeClient::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
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
				println!("{}", serde_json::to_string_pretty(&models)?);
			} else {
				println!("[{}] {} models", client.name(), models.len());
				for m in models.iter().take(limit) {
					let eps = m
						.supported_endpoints
						.as_deref()
						.map(|e| e.join(","))
						.unwrap_or_else(|| "-".into());
					println!("- {} (ctx={}, endpoints={})", m.id, m.context_length, eps);
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
		} => {
			run_chat(&*client, model, prompt, system, max_tokens, stream, route).await?;
		}
	}
	Ok(())
}

/// ハーネス本体の例: 具象型ではなく `&dyn LlmClient` / `impl LlmClient` に依存する.
/// この関数は commandcode / mock のどちらでも動く.
async fn run_chat(
	client: &dyn LlmClient,
	model: String,
	prompt: String,
	system: Option<String>,
	max_tokens: Option<u32>,
	stream: bool,
	route: String,
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
		let text = client
			.messages(&model, msgs, system.as_deref(), max_tokens.unwrap_or(1024))
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		println!("{text}");
	} else if wire == "responses" && !stream {
		let input = msgs.iter().map(|m| m.content.clone()).collect::<Vec<_>>().join("\n");
		let text = client
			.responses(&model, &input)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		println!("{text}");
	} else if stream {
		let mut stdout = std::io::stdout();
		let mut on_delta = |d: &str| {
			print!("{d}");
			let _ = stdout.flush();
		};
		client
			.chat_stream(&model, msgs, &mut on_delta)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		println!();
	} else {
		let text = client
			.chat(&model, msgs, max_tokens)
			.await
			.map_err(|e| anyhow::anyhow!("{e}"))?;
		println!("{text}");
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 差し替え可能性の証明: 同じジェネリック関数が両実装で動く
	async fn echo_hello(client: &impl LlmClient) -> String {
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
		let dyn_client: Arc<dyn LlmClient> = Arc::new(MockClient::new());
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
