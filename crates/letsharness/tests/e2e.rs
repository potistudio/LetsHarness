//! Client → Kernel → 外部プラグイン (別プロセス) の通しテスト。
//!
//! Client は kernel にしか依存しないので、プラグインのバイナリは別途ビルドして
//! `crates/*/plugin.toml` から発見させる。

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Once;

const PLUGINS: &[&str] = &[
	"letsharness-provider-mock",
	"letsharness-tool-local",
	"letsharness-agent-loop",
	"letsharness-plugin-dotenv",
	"letsharness-agent-antigravity",
];

fn plugin_dir() -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn build_plugins() {
	static ONCE: Once = Once::new();
	ONCE.call_once(|| {
		let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
		let mut cmd = Command::new(cargo);
		cmd.args(["build", "-q"]);
		for p in PLUGINS {
			cmd.args(["-p", p]);
		}
		assert!(cmd.status().expect("cargo build").success(), "プラグインのビルドに失敗");
	});
}

fn letsharness(args: &[&str]) -> Output {
	build_plugins();
	Command::new(env!("CARGO_BIN_EXE_letsharness"))
		.args(args)
		.env("LETSHARNESS_PLUGIN_PATH", plugin_dir())
		.current_dir(std::env::temp_dir())
		.output()
		.expect("run letsharness")
}

fn stdout(o: &Output) -> String {
	String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
	String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn chat_goes_through_an_external_provider() {
	let out = letsharness(&["--provider", "mock", "chat", "--model", "m", "--prompt", "hi"]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(stdout(&out).trim(), "[mock] m: hi");
}

#[test]
fn provider_options_reach_the_plugin() {
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		"prefix=>> ",
		"chat",
		"--model",
		"m",
		"--prompt",
		"hi",
	]);
	assert_eq!(stdout(&out).trim(), ">> m: hi");
}

#[test]
fn streaming_deltas_are_forwarded() {
	let out = letsharness(&[
		"--provider",
		"mock",
		"chat",
		"--model",
		"m",
		"--prompt",
		"hi",
		"--stream",
	]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(stdout(&out).contains("[mock] m: hi"), "{}", stdout(&out));
}

#[test]
fn plugin_errors_come_back_as_errors() {
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		"error=boom",
		"chat",
		"--model",
		"m",
		"--prompt",
		"hi",
	]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("mock: boom"), "{}", stderr(&out));
}

#[test]
fn unknown_provider_lists_what_the_kernel_found() {
	let out = letsharness(&["--provider", "nope", "models"]);
	assert!(!out.status.success());
	let err = stderr(&out);
	assert!(
		err.contains("unknown provider: nope (") && err.contains("mock"),
		"{err}"
	);
}

#[test]
fn kernel_runs_without_any_plugin() {
	build_plugins();
	let out = Command::new(env!("CARGO_BIN_EXE_letsharness"))
		.args(["--provider", "mock", "models"])
		.env("LETSHARNESS_PLUGIN_PATH", "/nonexistent")
		.output()
		.unwrap();
	assert!(!out.status.success());
	assert!(stderr(&out).contains("unknown provider: mock ()"), "{}", stderr(&out));
}

#[test]
fn loop_agent_borrows_model_and_tools_through_the_kernel() {
	let dir = std::env::temp_dir().join(format!("lh-e2e-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let out = letsharness(&[
		"--provider",
		"mock",
		"agent",
		"--model",
		"m",
		"--prompt",
		r#"tool:write_file {"path":"a.txt","content":"from tool"}"#,
		"--workdir",
		dir.to_str().unwrap(),
		"--yes",
	]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "from tool");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn dotenv_boot_options_are_validated() {
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		"env_file=/definitely/missing/.env",
		"models",
	]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("dotenv:"), "{}", stderr(&out));
}

/// 外部エージェント (偽の `agy`) の途中経過と結果が、プロセスをまたいで Client に届く。
#[cfg(unix)]
#[test]
fn external_agent_events_cross_the_process_boundary() {
	use std::os::unix::fs::PermissionsExt;

	build_plugins();
	let dir = std::env::temp_dir().join(format!("lh-e2e-agy-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let agy = dir.join("agy");
	std::fs::write(
		&agy,
		r#"#!/bin/sh
cat <<'EOF'
{"event":"init","conversation_id":"conv-1","init":{"cwd":"/tmp","tools":["read_file"],"permission_mode":"request-review"}}
{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"thinking","text_delta":"thinking-part"}}
{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"path":"a"}}}}
{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"path":"a"},"output":"A"}}}
{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"Hello from agy","num_turns":1}}
EOF
"#,
	)
	.unwrap();
	std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();

	let out = Command::new(env!("CARGO_BIN_EXE_letsharness"))
		.args(["--backend", "antigravity", "agent", "--prompt", "hi", "--workdir"])
		.arg(&dir)
		.env("LETSHARNESS_PLUGIN_PATH", plugin_dir())
		.env("AGY_BIN", &agy)
		.output()
		.unwrap();
	let (so, se) = (stdout(&out), stderr(&out));
	assert!(out.status.success(), "{se}");
	assert!(so.contains("Hello from agy"), "{so}");
	assert!(se.contains("read_file"), "tool call should be reported: {se}");
	std::fs::remove_dir_all(&dir).unwrap();
}
