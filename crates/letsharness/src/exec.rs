//! `loop` / `agent` サブコマンドの実行: ツールを使いながら最終回答まで進める。
//!
//! 途中経過 (ツール呼び出し・結果・承認の確認) はすべて stderr、最終回答だけが stdout。

use std::sync::Arc;

use anyhow::{Result, bail};
use letsharness_kernel::{self as llm, AgentBackend, ChatMessage, Host};
use serde_json::Value;

use crate::format_usage;

pub struct Options {
	/// 空ならバックエンドの既定モデル (`loop` では必須)。
	pub model: String,
	pub prompt: String,
	pub system: Option<String>,
	pub max_tokens: Option<u32>,
	pub max_steps: usize,
	/// 承認が必要なツールを確認なしで実行する。
	pub yes: bool,
	pub no_thinking: bool,
	pub show_usage: bool,
	/// 外部エージェントの会話 ID (`agent` のみ)。
	pub resume: Option<String>,
}

impl Options {
	fn messages(&self) -> Vec<ChatMessage> {
		let mut messages = Vec::new();
		if let Some(s) = &self.system {
			messages.push(ChatMessage::system(s));
		}
		messages.push(ChatMessage::user(&self.prompt));
		messages
	}

	fn approval(&self) -> llm::ApprovalPolicy {
		if self.yes {
			llm::ApprovalPolicy::AutoApprove
		} else {
			llm::ApprovalPolicy::Ask
		}
	}
}

/// kernel のループを回す (`--provider` のモデル + ツール)。
pub async fn run_loop(host: &Host, provider: &str, cfg: &Value, opts: Options) -> Result<()> {
	if opts.model.is_empty() {
		bail!("loop には --model が必要です");
	}
	let mut req = llm::LoopRequest::new(opts.model.clone(), opts.messages());
	req.max_tokens = opts.max_tokens;
	req.max_steps = opts.max_steps;
	req.approval = opts.approval();
	let mut handler = CliHandler {
		no_thinking: opts.no_thinking,
	};

	let out = host.run_loop(provider, cfg, req, &mut handler).await?;
	report(
		&opts,
		&out.text,
		out.usage.as_ref(),
		out.steps,
		None,
		out.hit_step_limit,
	)
}

/// 外部エージェントに丸ごと任せる。
pub async fn run_agent(backend: Arc<dyn AgentBackend>, opts: Options) -> Result<()> {
	let mut req = llm::AgentRequest::new(opts.model.clone(), opts.messages());
	req.max_tokens = opts.max_tokens;
	req.max_steps = opts.max_steps;
	req.session_id = opts.resume.clone();
	req.approval = opts.approval();
	if !backend.asks_approval() && !opts.yes {
		eprintln!(
			"(注: {name} はツール実行の承認をこちらに求めません。{name} 側の権限設定で許可されていない操作は却下されます。全許可は --yes)",
			name = backend.name()
		);
	}
	let mut handler = CliHandler {
		no_thinking: opts.no_thinking,
	};

	let out = backend.run(req, &mut handler).await?;
	report(
		&opts,
		&out.text,
		out.usage.as_ref(),
		out.steps,
		out.session_id.as_deref(),
		out.hit_step_limit,
	)
}

/// 最終回答だけを stdout へ (途中経過は stderr なのでパイプ可能)。
fn report(
	opts: &Options,
	text: &str,
	usage: Option<&llm::Usage>,
	steps: usize,
	session_id: Option<&str>,
	hit_step_limit: bool,
) -> Result<()> {
	println!("{text}");
	if opts.show_usage {
		match usage {
			Some(u) => eprintln!("{} steps={steps}", format_usage(u)),
			None => eprintln!("usage: (none) steps={steps}"),
		}
	}
	if let Some(id) = session_id {
		eprintln!("session: {id} (--resume {id} で続きから)");
	}
	if hit_step_limit {
		bail!(
			"--max-steps={} に達したため打ち切りました (タスクは未完了の可能性があります)",
			opts.max_steps
		);
	}
	Ok(())
}

/// 表示・承認を担う Handler。
struct CliHandler {
	no_thinking: bool,
}

/// 1行に収めた表示用の要約 (長ければ省略)。
fn one_line(s: &str, max: usize) -> String {
	let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
	match flat.char_indices().nth(max) {
		Some((i, _)) => format!("{}…", &flat[..i]),
		None => flat,
	}
}

/// 結果表示の上限 (超えた分は省略して件数を示す)。モデルへ返す内容はこの上限に影響されない。
const RESULT_MAX_LINES: usize = 30;
const RESULT_MAX_CHARS: usize = 3000;

/// ツール結果を字下げした複数行で整形する。長すぎる場合は先頭から上限まで出して省略を明示する。
fn format_result(content: &str) -> String {
	if content.trim().is_empty() {
		return "    (空)".into();
	}
	let mut out = String::new();
	let mut chars = 0;
	let mut shown = 0;
	let total = content.lines().count();
	for line in content.lines() {
		let remaining = RESULT_MAX_CHARS - chars;
		if shown == RESULT_MAX_LINES || remaining == 0 {
			break;
		}
		let line = match line.char_indices().nth(remaining) {
			Some((i, _)) => &line[..i],
			None => line,
		};
		chars += line.chars().count();
		out.push_str("    ");
		out.push_str(line);
		out.push('\n');
		shown += 1;
	}
	if shown < total {
		out.push_str(&format!("    … (残り {} 行を省略: 全 {total} 行)", total - shown));
	} else {
		out.pop(); // 末尾の改行
	}
	out
}

#[async_trait::async_trait]
impl llm::RunHandler for CliHandler {
	fn on_text(&mut self, text: &str) {
		eprintln!("{text}");
	}

	fn on_thinking(&mut self, thinking: &str) {
		if !self.no_thinking {
			eprintln!("--- thinking ---\n{thinking}\n--- end of thinking ---");
		}
	}

	fn on_tool_call(&mut self, call: &llm::ToolCall) {
		eprintln!("▶ {} {}", call.name, one_line(&call.arguments.to_string(), 200));
	}

	fn on_tool_result(&mut self, _call: &llm::ToolCall, output: &llm::ToolOutput) {
		let mark = if output.is_error { "✗ error" } else { "✓ ok" };
		eprintln!("  {mark}\n{}", format_result(&output.content));
	}

	// `--yes` は ApprovalPolicy::AutoApprove になり、ここは呼ばれない。
	async fn approve(&mut self, call: &llm::ToolCall, _spec: &llm::ToolSpec) -> bool {
		eprint!("  {} を実行する? [y/N] ", call.name);
		// stdin はブロッキングなので別スレッドで待つ。EOF (非対話) は拒否扱い。
		tokio::task::spawn_blocking(|| {
			let mut line = String::new();
			std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
		})
		.await
		.unwrap_or(false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_line_flattens_and_truncates_on_char_boundary() {
		assert_eq!(one_line("a\n  b\tc", 10), "a b c");
		assert_eq!(one_line(&"あ".repeat(5), 3), "あああ…");
	}

	#[test]
	fn format_result_indents_and_marks_omission() {
		assert_eq!(format_result("a\nb"), "    a\n    b");
		assert_eq!(format_result("  \n"), "    (空)");
		let long = (1..=40).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
		let out = format_result(&long);
		assert!(out.ends_with("… (残り 10 行を省略: 全 40 行)"), "{out}");
		assert_eq!(out.lines().count(), RESULT_MAX_LINES + 1);
		// 1行が巨大でも文字数上限で切る (文字境界を守る)
		let wide = "あ".repeat(RESULT_MAX_CHARS + 10);
		assert!(format_result(&wide).chars().count() < RESULT_MAX_CHARS + 20);
	}
}
