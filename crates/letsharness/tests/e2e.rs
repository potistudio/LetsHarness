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
	"letsharness-plugin-dotenv",
	"letsharness-agent-antigravity",
	"letsharness-hook-guard",
	"letsharness-observer-log",
	"letsharness-tool-mcp",
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
		// MCP のテスト用サーバー (examples はプラグインと別に指定する)
		let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
		cmd.args(["build", "-q", "-p", "letsharness-tool-mcp", "--example", "mcp-echo"]);
		assert!(cmd.status().expect("cargo build").success(), "mcp-echo のビルドに失敗");
	});
}

/// テスト用の MCP サーバー (`crates/tool-mcp/examples/mcp-echo.rs`)。
fn mcp_echo() -> PathBuf {
	let bin = PathBuf::from(env!("CARGO_BIN_EXE_letsharness"));
	bin.parent().unwrap().join("examples").join("mcp-echo")
}

/// profile を持たない LetsHarness の置き場 (利用者の `~/.letsharness` を読み書きしないため)。セッションログはここに溜まる。
fn scratch_home() -> PathBuf {
	std::env::temp_dir().join(format!("lh-e2e-home-{}", std::process::id()))
}

fn letsharness(args: &[&str]) -> Output {
	letsharness_in(&scratch_home(), args)
}

/// `home` を `$LETSHARNESS_HOME` にして動かす (profile を読み書きするテスト用)。
fn letsharness_in(home: &std::path::Path, args: &[&str]) -> Output {
	build_plugins();
	Command::new(env!("CARGO_BIN_EXE_letsharness"))
		.args(args)
		.env("LETSHARNESS_PLUGIN_PATH", plugin_dir())
		.env("LETSHARNESS_HOME", home)
		.env_remove("LETSHARNESS_PROFILE")
		.env_remove("LETSHARNESS_CONFIG")
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
		.env("LETSHARNESS_HOME", scratch_home())
		.output()
		.unwrap();
	assert!(!out.status.success());
	assert!(stderr(&out).contains("unknown provider: mock ()"), "{}", stderr(&out));
}

#[test]
fn kernel_loop_runs_external_provider_and_external_tools() {
	let dir = std::env::temp_dir().join(format!("lh-e2e-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let out = letsharness(&[
		"--provider",
		"mock",
		"loop",
		"--model",
		"m",
		"--prompt",
		r#"tool:write_file {"path":"a.txt","content":"from tool"}"#,
		"--workdir",
		dir.to_str().unwrap(),
	]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "from tool");
	std::fs::remove_dir_all(&dir).unwrap();
}

/// 外部プラグインの Hook がループに割り込み、ツールを実行させずに結果を差し込む。
#[test]
fn hook_plugin_blocks_a_tool_call() {
	let dir = std::env::temp_dir().join(format!("lh-e2e-hook-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		"deny_tools=write_file",
		"loop",
		"--model",
		"m",
		"--prompt",
		r#"tool:write_file {"path":"a.txt","content":"from tool"}"#,
		"--workdir",
		dir.to_str().unwrap(),
	]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(!dir.join("a.txt").exists(), "blocked tool must not run");
	assert!(
		stdout(&out).contains("tool `write_file` is blocked by deny_tools"),
		"{}",
		stdout(&out)
	);
	std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hook_config_is_validated_against_the_tools() {
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		"deny_tools=no_such_tool",
		"loop",
		"--model",
		"m",
		"--prompt",
		"hi",
	]);
	assert!(!out.status.success());
	assert!(
		stderr(&out).contains("deny_tools: unknown tool `no_such_tool`"),
		"{}",
		stderr(&out)
	);
}

/// 外部プラグインの Observer に、ループの出来事が起きた順で届く。
#[test]
fn observer_plugin_sees_loop_events_in_order() {
	let dir = std::env::temp_dir().join(format!("lh-e2e-observer-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let log = dir.join("events.jsonl");
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		&format!("event_log={}", log.display()),
		"loop",
		"--model",
		"m",
		"--prompt",
		r#"tool:list_dir {"path":"."}"#,
		"--workdir",
		dir.to_str().unwrap(),
	]);
	assert!(out.status.success(), "{}", stderr(&out));
	let text = std::fs::read_to_string(&log).unwrap();
	let kinds: Vec<String> = text
		.lines()
		.map(|l| {
			serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
				.as_str()
				.unwrap()
				.to_owned()
		})
		.collect();
	assert_eq!(kinds, ["request", "response", "tool_result", "request", "response"]);
	std::fs::remove_dir_all(&dir).unwrap();
}

/// `chat` もループと同じく Session を通るので、Observer に届く。
#[test]
fn chat_goes_through_the_session() {
	let log = std::env::temp_dir().join(format!("lh-e2e-chat-{}.jsonl", std::process::id()));
	let _ = std::fs::remove_file(&log);
	for stream in [false, true] {
		let mut args = vec![
			"--provider".to_string(),
			"mock".into(),
			"-o".into(),
			format!("event_log={}", log.display()),
			"chat".into(),
			"--model".into(),
			"m".into(),
			"--prompt".into(),
			"hi".into(),
		];
		if stream {
			args.push("--stream".into());
		}
		let out = letsharness(&args.iter().map(String::as_str).collect::<Vec<_>>());
		assert!(out.status.success(), "{}", stderr(&out));
		assert_eq!(stdout(&out).trim(), "[mock] m: hi");
	}
	let text = std::fs::read_to_string(&log).unwrap();
	let kinds: Vec<String> = text
		.lines()
		.map(|l| {
			serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
				.as_str()
				.unwrap()
				.to_owned()
		})
		.collect();
	assert_eq!(kinds, ["request", "response", "request", "response"]);
	std::fs::remove_file(&log).unwrap();
}

/// `plugin disable` は profile に書かれ、以降の起動でそのプラグインは使われない。`enable` で跡が消える。
#[test]
fn plugins_are_toggled_in_the_profile() {
	let home = std::env::temp_dir().join(format!("lh-e2e-profile-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&home);
	let patch = home.join("profiles/default/patch.toml");

	let out = letsharness_in(&home, &["plugin", "disable", "guard"]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(stdout(&out).starts_with("guard: disabled"), "{}", stdout(&out));
	let list = stdout(&letsharness_in(&home, &["plugin", "list"]));
	assert!(list.contains("off guard [hook:guard]"), "{list}");

	// 無効な guard は deny_tools を見ないので、ツールが実行される
	let dir = home.join("work");
	std::fs::create_dir_all(&dir).unwrap();
	let out = letsharness_in(
		&home,
		&[
			"--provider",
			"mock",
			"-o",
			"deny_tools=write_file",
			"loop",
			"--model",
			"m",
			"--prompt",
			r#"tool:write_file {"path":"a.txt","content":"x"}"#,
			"--workdir",
			dir.to_str().unwrap(),
		],
	);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(dir.join("a.txt").exists());

	let out = letsharness_in(&home, &["plugin", "enable", "guard"]);
	assert!(stdout(&out).starts_with("guard: enabled"), "{}", stdout(&out));
	assert_eq!(std::fs::read_to_string(&patch).unwrap().trim(), "");

	let out = letsharness_in(&home, &["plugin", "disable", "nope"]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("unknown plugin: nope"), "{}", stderr(&out));
	std::fs::remove_dir_all(&home).unwrap();
}

/// パッチの `config` がプラグインに届き、`-o` がそれより強い。知らない id は起動を止める。
#[test]
fn patch_layers_configure_plugins() {
	let dir = std::env::temp_dir().join(format!("lh-e2e-patch-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let patch = dir.join("mock.toml");
	std::fs::write(&patch, "[[plugins]]\nid = \"mock\"\nconfig = { prefix = \"P> \" }\n").unwrap();
	let patch = patch.to_str().unwrap();

	let chat = [
		"--provider",
		"mock",
		"--patch",
		patch,
		"chat",
		"--model",
		"m",
		"--prompt",
		"hi",
	];
	let out = letsharness(&chat);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(stdout(&out).trim(), "P> m: hi");
	let out = letsharness(&[&["-o", "prefix=>> "][..], &chat].concat());
	assert_eq!(stdout(&out).trim(), ">> m: hi");

	let bad = dir.join("bad.toml");
	std::fs::write(&bad, "[[plugins]]\nid = \"nope\"\n").unwrap();
	let out = letsharness(&["--patch", bad.to_str().unwrap(), "--provider", "mock", "models"]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("unknown plugin `nope`"), "{}", stderr(&out));
	std::fs::remove_dir_all(&dir).unwrap();
}

/// MCP サーバーのツールは `mcp__<server>__<tool>` としてループで使える。
#[test]
fn mcp_server_tools_run_in_the_loop() {
	build_plugins();
	let dir = std::env::temp_dir().join(format!("lh-e2e-mcp-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let servers = serde_json::json!({
		"echo": { "command": mcp_echo(), "env": { "MCP_ECHO_TAG": "${LH_E2E_TAG:-tagged}" } },
	});
	let run = |prompt: &str| {
		letsharness(&[
			"--provider",
			"mock",
			"-o",
			&format!("mcp_servers={servers}"),
			"loop",
			"--model",
			"m",
			"--workdir",
			dir.to_str().unwrap(),
			"--prompt",
			prompt,
		])
	};

	let out = run(r#"tool:mcp__echo__echo {"text":"hello mcp"}"#);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(stdout(&out).trim(), "[mock] tool result: hello mcp");
	// 2ページ目のツールも見える。作業ディレクトリは --workdir、env の ${VAR:-既定} は展開される
	let out = run("tool:mcp__echo__where");
	let text = stdout(&out);
	let canon = dir.canonicalize().unwrap();
	assert!(
		text.contains(canon.to_str().unwrap()) && text.trim_end().ends_with("tagged"),
		"{text}"
	);
	let out = run("tool:mcp__echo__fail");
	assert_eq!(stdout(&out).trim(), "[mock] tool result: Error: it failed");

	// つなげないサーバーは黙って飛ばさない
	let out = letsharness(&[
		"--provider",
		"mock",
		"-o",
		r#"mcp_servers={"bad": {"command": "/no/such/mcp-server"}}"#,
		"loop",
		"--model",
		"m",
		"--prompt",
		"hi",
	]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("bad: /no/such/mcp-server"), "{}", stderr(&out));
	std::fs::remove_dir_all(&dir).unwrap();
}

/// `plugin install` は plugin.toml と実行ファイルを `<home>/plugins` に写し、どこからでも発見されるようにする。
#[test]
fn plugins_are_installed_into_the_home() {
	build_plugins();
	let root = std::env::temp_dir().join(format!("lh-e2e-install-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&root);
	let (home, empty) = (root.join("home"), root.join("empty"));
	std::fs::create_dir_all(&empty).unwrap();
	// 開発中の crates/ を探索先から外し、導入先だけで動かす
	let run = |args: &[&str]| {
		Command::new(env!("CARGO_BIN_EXE_letsharness"))
			.args(args)
			.env("LETSHARNESS_PLUGIN_PATH", &empty)
			.env("LETSHARNESS_HOME", &home)
			.env_remove("LETSHARNESS_PROFILE")
			.env_remove("LETSHARNESS_CONFIG")
			.current_dir(std::env::temp_dir())
			.output()
			.expect("run letsharness")
	};
	let chat = || run(&["--provider", "mock", "chat", "--model", "m", "--prompt", "hi"]);
	assert!(!chat().status.success(), "nothing is installed yet");

	let mock = plugin_dir().join("provider-mock");
	let out = run(&["plugin", "install", mock.to_str().unwrap()]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(stdout(&out).contains("installed mock"), "{}", stdout(&out));
	assert!(home.join("plugins/mock/letsharness-provider-mock").is_file());
	assert_eq!(stdout(&chat()).trim(), "[mock] m: hi");

	let out = run(&["plugin", "install", mock.to_str().unwrap()]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("--force"), "{}", stderr(&out));
	assert!(
		run(&["plugin", "install", "--force", mock.to_str().unwrap()])
			.status
			.success()
	);

	let out = run(&["plugin", "uninstall", "mock"]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert!(!home.join("plugins/mock").exists());
	assert!(!chat().status.success());
	std::fs::remove_dir_all(&root).unwrap();
}

/// 設定ファイルの `[providers.NAME]` は名前付きのインスタンスで、`default_provider` は `--provider` の既定になる。
#[test]
fn the_config_file_names_providers() {
	let home = std::env::temp_dir().join(format!("lh-e2e-config-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&home);
	std::fs::create_dir_all(&home).unwrap();
	std::fs::write(
		home.join("config.toml"),
		"default_provider = \"echo\"\n[providers.echo]\nkind = \"mock\"\nprefix = \"cfg> \"\n[providers.bad]\nkind = \"nope\"\n",
	)
	.unwrap();
	let chat = |extra: &[&str]| {
		letsharness_in(
			&home,
			&[&["chat", "--model", "m", "--prompt", "hi"][..], extra].concat(),
		)
	};

	let out = chat(&[]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(stdout(&out).trim(), "cfg> m: hi");
	// -o は設定ファイルより強い
	let out = chat(&["-o", "prefix=cli> "]);
	assert_eq!(stdout(&out).trim(), "cli> m: hi");
	// 表に無い名前は kind
	let out = chat(&["--provider", "mock"]);
	assert_eq!(stdout(&out).trim(), "[mock] m: hi");
	// 再開はログに残った名前で、設定ファイルの表をまた使う
	let id = stderr(&chat(&[]))
		.lines()
		.find_map(|l| l.strip_prefix("session: "))
		.and_then(|l| l.split_whitespace().next())
		.expect("session id")
		.to_owned();
	let out = chat(&["--resume", &id]);
	assert_eq!(stdout(&out).trim(), "cfg> m: hi");

	let out = chat(&["--provider", "bad"]);
	assert!(!out.status.success());
	assert!(
		stderr(&out).contains("providers.bad: unknown kind: nope"),
		"{}",
		stderr(&out)
	);
	let out = chat(&["--provider", "nope"]);
	assert!(
		stderr(&out).contains("unknown provider: nope (bad|echo|"),
		"{}",
		stderr(&out)
	);
	let out = chat(&["--config", home.join("missing.toml").to_str().unwrap()]);
	assert!(stderr(&out).contains("ファイルがありません"), "{}", stderr(&out));

	std::fs::write(home.join("config.toml"), "default_providr = \"x\"\n").unwrap();
	let out = chat(&[]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("unknown field"), "{}", stderr(&out));
	std::fs::remove_dir_all(&home).unwrap();
}

/// 会話はセッションログに残り、`--resume` で前のターンを含めて続けられる。
#[test]
fn conversations_are_logged_and_resumed() {
	let home = std::env::temp_dir().join(format!("lh-e2e-resume-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&home);
	let chat = |extra: &[&str]| {
		let args = [&["--provider", "mock", "chat", "--model", "m"][..], extra].concat();
		letsharness_in(&home, &args)
	};

	let out = chat(&["--prompt", "first"]);
	assert!(out.status.success(), "{}", stderr(&out));
	let err = stderr(&out);
	let id = err
		.lines()
		.find_map(|l| l.strip_prefix("session: "))
		.and_then(|l| l.split_whitespace().next())
		.unwrap_or_else(|| panic!("session id is reported: {err}"))
		.to_owned();

	let out = chat(&["--prompt", "second", "--resume", &id]);
	assert!(out.status.success(), "{}", stderr(&out));
	assert_eq!(stdout(&out).trim(), "[mock] m: second");

	let log = std::fs::read_to_string(home.join(format!("sessions/{id}.jsonl"))).unwrap();
	let records: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
	let of = |t: &str| records.iter().filter(|r| r["type"] == t).collect::<Vec<_>>();
	assert_eq!(of("session").len(), 1);
	let messages: Vec<&str> = of("message")
		.iter()
		.map(|r| r["message"]["content"].as_str().unwrap())
		.collect();
	assert_eq!(messages, ["first", "[mock] m: first", "second", "[mock] m: second"]);
	// 再開したターンのリクエストには前のターンが含まれる
	let last_request = of("event").into_iter().rfind(|r| r["event"] == "request").unwrap();
	assert_eq!(last_request["request"]["messages"].as_array().unwrap().len(), 3);

	let out = chat(&["--prompt", "x", "--resume", &id, "--system", "s"]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("--system は新しい会話でだけ"), "{}", stderr(&out));
	let out = chat(&["--prompt", "x", "--resume", "no-such-session"]);
	assert!(!out.status.success());
	assert!(stderr(&out).contains("session no-such-session"), "{}", stderr(&out));
	std::fs::remove_dir_all(&home).unwrap();
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
		.env("LETSHARNESS_HOME", scratch_home())
		.output()
		.unwrap();
	let (so, se) = (stdout(&out), stderr(&out));
	assert!(out.status.success(), "{se}");
	assert!(so.contains("Hello from agy"), "{so}");
	assert!(se.contains("read_file"), "tool call should be reported: {se}");
	std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn agent_needs_an_explicit_backend() {
	let out = letsharness(&["--provider", "mock", "agent", "--prompt", "hi"]);
	assert!(!out.status.success());
	assert!(
		stderr(&out).contains("agent には --backend が必要です"),
		"{}",
		stderr(&out)
	);
}
