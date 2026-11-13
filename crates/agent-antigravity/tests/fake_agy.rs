//! 偽の `agy` (シェルスクリプト) でサブプロセス経路を通すテスト。
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use letsharness_agent_antigravity::AntigravityAgent;
use letsharness_kernel::{
	AgentBackend, AgentHandler, AgentRequest, ApprovalPolicy, ChatMessage, Error, ToolCall, ToolOutput, ToolSpec,
};

/// `body` を本文とする実行可能スクリプトを作る。受け取った引数は `<name>.args` に1行ずつ残す。
fn fake_agy(name: &str, body: &str) -> (AntigravityAgent, PathBuf) {
	let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fake-agy");
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join(name);
	let args = dir.join(format!("{name}.args"));
	let script = format!(
		"#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n{body}\n",
		args.display()
	);
	std::fs::write(&path, script).unwrap();
	std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
	(AntigravityAgent::new(&path), args)
}

fn read_args(path: &PathBuf) -> Vec<String> {
	std::fs::read_to_string(path)
		.unwrap()
		.lines()
		.map(str::to_string)
		.collect()
}

/// 通知を記録し、承認を求められたら失敗させるハンドラ (外部エージェントは approve を呼ばない)。
#[derive(Default)]
struct Rec(Vec<String>);

#[async_trait::async_trait]
impl AgentHandler for Rec {
	fn on_text(&mut self, t: &str) {
		self.0.push(format!("text:{t}"));
	}
	fn on_thinking(&mut self, t: &str) {
		self.0.push(format!("thinking:{t}"));
	}
	fn on_tool_call(&mut self, c: &ToolCall) {
		self.0.push(format!("call:{}", c.name));
	}
	fn on_tool_result(&mut self, c: &ToolCall, o: &ToolOutput) {
		self.0.push(format!("result:{}:{}", c.name, o.content));
	}
	async fn approve(&mut self, _: &ToolCall, _: &ToolSpec) -> bool {
		panic!("外部エージェントでは approve は呼ばれない");
	}
}

const SUCCESS: &str = r#"cat <<'EOF'
{"event":"init","conversation_id":"conv-1","init":{"cwd":"/tmp","tools":["read_file"],"permission_mode":"request-review"}}
{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":0,"state":"DONE","step_type":"user_input","text_delta":"echo"}}
{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"thinking","text_delta":"考え中"}}
{"event":"step_update","step_update":{"step_index":2,"state":"DONE","step_type":"agent_response","text_delta":"読むで"}}
{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"path":"a"}}}}
{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"path":"a"},"output":"A"}}}
{"event":"step_update","step_update":{"step_index":4,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hel"}}
{"event":"step_update","step_update":{"step_index":4,"state":"DONE","step_type":"agent_response","text_delta":"lo"}}
{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"Hello","num_turns":1,"usage":{"input_tokens":10,"output_tokens":2,"thinking_tokens":3,"total_tokens":15}}}
EOF
echo "progress..." >&2"#;

#[tokio::test]
async fn runs_and_reports_steps() {
	let (agy, args_file) = fake_agy("success", SUCCESS);
	assert!(!agy.asks_approval());
	let mut h = Rec::default();
	let req = AgentRequest::new("gemini-x", vec![ChatMessage::system("sys"), ChatMessage::user("-q")]);
	let out = agy.run(req, &mut h).await.unwrap();

	assert_eq!(out.text, "Hello");
	assert_eq!(out.steps, 2);
	assert_eq!(out.session_id.as_deref(), Some("conv-1"));
	assert_eq!(out.messages.last().unwrap().content, "Hello");
	assert_eq!(
		h.0,
		["thinking:考え中", "text:読むで", "call:read_file", "result:read_file:A"]
	);
	let u = out.usage.unwrap();
	assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens), (10, 5, 15));
	assert_eq!(u.request_id.as_deref(), Some("conv-1"));

	let args = read_args(&args_file);
	assert_eq!(
		args[..6],
		[
			"--model",
			"gemini-x",
			"--output-format",
			"stream-json",
			"--disable-slash-commands",
			"-p"
		]
	);
	// system はプロンプトに畳まれ、`-` 始まりの本文もそのまま値として渡る
	assert_eq!(args[6..].join("\n"), "<system>\nsys\n</system>\n\n-q");
}

#[tokio::test]
async fn resume_and_auto_approve_flags() {
	let (agy, args_file) = fake_agy(
		"resume",
		r#"echo '{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"続き"}}'"#,
	);
	let mut req = AgentRequest::new(
		"",
		vec![
			ChatMessage::user("q1"),
			ChatMessage::assistant("a1"),
			ChatMessage::user("q2"),
		],
	);
	req.session_id = Some("conv-1".into());
	req.approval = ApprovalPolicy::AutoApprove;
	let out = agy.run(req, &mut Rec::default()).await.unwrap();
	assert_eq!((out.text.as_str(), out.steps), ("続き", 1));
	assert!(out.usage.is_none());
	// 空モデルは --model を付けない / 再開時は最後の依頼だけ送る
	assert_eq!(
		read_args(&args_file),
		[
			"--conversation",
			"conv-1",
			"--dangerously-skip-permissions",
			"--output-format",
			"stream-json",
			"--disable-slash-commands",
			"-p",
			"q2"
		]
	);
}

#[tokio::test]
async fn falls_back_to_streamed_text() {
	let (agy, _) = fake_agy(
		"no-response",
		r#"echo '{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"streamed"}}'
echo '{"event":"result","result":{"status":"SUCCESS"}}'"#,
	);
	let out = agy
		.run(
			AgentRequest::new("m", vec![ChatMessage::user("q")]),
			&mut Rec::default(),
		)
		.await
		.unwrap();
	assert_eq!(out.text, "streamed");
}

#[tokio::test]
async fn errors_are_reported() {
	let req = || AgentRequest::new("m", vec![ChatMessage::user("q")]);
	let (agy, _) = fake_agy(
		"error",
		r#"echo '{"event":"result","result":{"status":"ERROR","error":"unknown model"}}'; echo detail >&2; exit 1"#,
	);
	let e = agy.run(req(), &mut Rec::default()).await.unwrap_err().to_string();
	assert!(e.contains("status=ERROR: unknown model"), "{e}");
	assert!(e.contains("detail"), "{e}");

	let (agy, _) = fake_agy("auth", "echo 'Error: authentication required' >&2; exit 1");
	let e = agy.run(req(), &mut Rec::default()).await.unwrap_err();
	assert!(matches!(e, Error::Config(_)), "{e}");

	let missing = AntigravityAgent::new("/nonexistent/agy");
	let e = missing.run(req(), &mut Rec::default()).await.unwrap_err();
	assert!(matches!(&e, Error::Config(m) if m.contains("install.sh")), "{e}");
}

#[tokio::test]
async fn lists_models() {
	let (agy, args_file) = fake_agy("models", "printf 'Models:\\ngemini-a\\n  gemini-b (default)\\n'");
	let models = agy.list_models().await.unwrap();
	let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
	assert_eq!(ids, ["gemini-a", "gemini-b"]);
	assert_eq!(read_args(&args_file), ["models"]);
}
