//! ローカルツールを公開API (`local_tools`) だけで通す結合テスト。

use std::path::PathBuf;
use std::sync::Arc;

use letsharness_kernel::Tool;
use letsharness_tool_local::local_tools;

/// テストごとに使い捨ての作業ディレクトリを作る。
struct TempRoot(PathBuf);

impl TempRoot {
	fn new(tag: &str) -> Self {
		let dir = std::env::temp_dir().join(format!("letsharness-tool-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		Self(dir)
	}
	fn tool(&self, name: &str) -> Arc<dyn Tool> {
		local_tools(&self.0)
			.into_iter()
			.find(|t| t.spec().name == name)
			.unwrap()
	}
}

impl Drop for TempRoot {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

#[tokio::test]
async fn write_read_edit_roundtrip() {
	let t = TempRoot::new("rw");
	let w = t
		.tool("write_file")
		.call(serde_json::json!({"path": "sub/a.txt", "content": "one\ntwo\nthree\n"}))
		.await;
	assert!(!w.is_error, "{}", w.content);

	let r = t.tool("read_file").call(serde_json::json!({"path": "sub/a.txt"})).await;
	assert_eq!(r.content, "1\tone\n2\ttwo\n3\tthree\n");

	let r = t
		.tool("read_file")
		.call(serde_json::json!({"path": "sub/a.txt", "offset": 2, "limit": 1}))
		.await;
	assert!(r.content.starts_with("2\ttwo\n"), "{}", r.content);
	assert!(r.content.contains("offset=3"), "{}", r.content);

	let e = t
		.tool("edit_file")
		.call(serde_json::json!({"path": "sub/a.txt", "old_string": "two", "new_string": "2"}))
		.await;
	assert!(!e.is_error, "{}", e.content);
	assert_eq!(
		std::fs::read_to_string(t.0.join("sub/a.txt")).unwrap(),
		"one\n2\nthree\n"
	);

	let l = t.tool("list_dir").call(serde_json::json!({})).await;
	assert_eq!(l.content, "sub/");
}

#[tokio::test]
async fn edit_requires_unique_match() {
	let t = TempRoot::new("edit");
	std::fs::write(t.0.join("f"), "x x").unwrap();
	let edit = t.tool("edit_file");
	let dup = edit
		.call(serde_json::json!({"path": "f", "old_string": "x", "new_string": "y"}))
		.await;
	assert!(dup.is_error && dup.content.contains("2 か所"), "{}", dup.content);
	let none = edit
		.call(serde_json::json!({"path": "f", "old_string": "z", "new_string": "y"}))
		.await;
	assert!(none.is_error);
	let all = edit
		.call(serde_json::json!({"path": "f", "old_string": "x", "new_string": "y", "replace_all": true}))
		.await;
	assert!(!all.is_error);
	assert_eq!(std::fs::read_to_string(t.0.join("f")).unwrap(), "y y");
}

#[tokio::test]
async fn paths_cannot_escape_root() {
	let t = TempRoot::new("escape");
	let outside = TempRoot::new("escape-outside");
	std::fs::write(outside.0.join("secret"), "s").unwrap();
	#[cfg(unix)]
	std::os::unix::fs::symlink(&outside.0, t.0.join("link")).unwrap();

	let read = t.tool("read_file");
	let mut attempts = vec![
		format!("../{}/secret", outside.0.file_name().unwrap().to_string_lossy()),
		"../../../../etc/passwd".to_string(),
		outside.0.join("secret").display().to_string(),
	];
	if cfg!(unix) {
		attempts.push("link/secret".into());
	}
	for p in attempts {
		let r = read.call(serde_json::json!({"path": p})).await;
		assert!(
			r.is_error && r.content.contains("外は扱えません"),
			"{p} が拒否されなかった: {}",
			r.content
		);
	}
	// 書き込みも同様 (存在しない祖先経由も塞ぐ)
	let w = t
		.tool("write_file")
		.call(serde_json::json!({"path": "../evil", "content": "x"}))
		.await;
	assert!(w.is_error);
	assert!(!t.0.parent().unwrap().join("evil").exists());
	// root 内の `..` 往復は許可
	std::fs::write(t.0.join("ok"), "fine").unwrap();
	let r = read.call(serde_json::json!({"path": "sub/../ok"})).await;
	assert!(!r.is_error, "{}", r.content);
}

#[tokio::test]
async fn bad_arguments_are_reported_not_panicking() {
	let t = TempRoot::new("args");
	let r = t.tool("read_file").call(serde_json::json!({"nope": 1})).await;
	assert!(
		r.is_error && r.content.starts_with("invalid arguments"),
		"{}",
		r.content
	);
	// プロバイダが壊れた引数文字列を Value::String で渡してきた場合
	let r = t.tool("bash").call(serde_json::json!("{oops")).await;
	assert!(r.is_error);
}

#[cfg(unix)]
#[tokio::test]
async fn bash_runs_in_root_and_reports_exit_code() {
	let t = TempRoot::new("bash");
	std::fs::write(t.0.join("marker"), "").unwrap();
	let bash = t.tool("bash");

	let ok = bash.call(serde_json::json!({"command": "ls; echo err >&2"})).await;
	assert!(!ok.is_error, "{}", ok.content);
	assert!(
		ok.content.contains("marker") && ok.content.contains("[stderr]\nerr"),
		"{}",
		ok.content
	);

	let fail = bash.call(serde_json::json!({"command": "echo hi; exit 3"})).await;
	assert!(
		fail.is_error && fail.content.starts_with("exit code 3"),
		"{}",
		fail.content
	);

	let slow = bash
		.call(serde_json::json!({"command": "sleep 5", "timeout_secs": 1}))
		.await;
	assert!(
		slow.is_error && slow.content.contains("タイムアウト"),
		"{}",
		slow.content
	);
}
