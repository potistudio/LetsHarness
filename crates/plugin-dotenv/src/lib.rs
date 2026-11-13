//! `.env` ファイルを読み込み、Provider などに渡す環境変数を kernel に返すプラグイン。
//!
//! Provider は `COMMAND_CODE_API_KEY` / `GEMINI_API_KEY` などを環境変数から読む。
//! このプラグインは kernel の `boot` で呼ばれ、読んだ値を返す。kernel がそれを
//! 以降に起動するプラグインの環境に足す (kernel 自身の環境変数は書き換えない)。
//!
//! - オプション (`-o key=value`): `env_file=<PATH>` で読むファイルを指定 /
//!   `no_env_file=true` で読み込まない。省略時はカレントから親へ遡って `.env` を探す。
//! - 既に設定されている環境変数は上書きしない (`環境変数 > .env`)。
//! - 対応する書式: `KEY=VALUE` / `export KEY=VALUE` / `#` コメント /
//!   `"..."` (エスケープ `\n \r \t \" \\` と複数行) / `'...'` (リテラル、複数行) /
//!   クォートなし値の行末コメント (`KEY=v # comment`)。
//! - `${VAR}` の展開はしない。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use letsharness_kernel::BootHook;
use serde_json::Value;

/// 既定のファイル名。
pub const FILE_NAME: &str = ".env";

/// `.env` を読み込む kernel プラグイン (`boot` フック)。
#[derive(Debug, Default, Clone, Copy)]
pub struct DotenvPlugin;

impl BootHook for DotenvPlugin {
	fn boot(&self, options: &Value) -> letsharness_kernel::Result<HashMap<String, String>> {
		let env_file = options.get("env_file").and_then(Value::as_str);
		let disabled = options
			.get("no_env_file")
			.and_then(Value::as_str)
			.is_some_and(|v| !matches!(v, "false" | "0"));
		if disabled {
			if env_file.is_some() {
				return Err(letsharness_kernel::Error::Config(
					"env_file と no_env_file は同時に指定できません".into(),
				));
			}
			return Ok(HashMap::new());
		}
		let loaded = load(env_file.map(Path::new)).map_err(|e| letsharness_kernel::Error::plugin("dotenv", e))?;
		Ok(loaded.map(|l| l.env.into_iter().collect()).unwrap_or_default())
	}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{path}: {source}")]
	Io {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
	#[error("{path}:{line}: {message}")]
	Parse {
		path: PathBuf,
		line: usize,
		message: String,
	},
}

/// [`parse`] のエラー。ファイルパスは [`load`] 側で付ける。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ParseError {
	pub line: usize,
	pub message: String,
}

/// [`load`] の結果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Loaded {
	/// 読み込んだファイル。
	pub path: PathBuf,
	/// 渡すべき環境変数 (既に環境にあるキーは含まない)。
	pub env: Vec<(String, String)>,
	/// 既に環境変数があったため無視したキー。
	pub skipped: Vec<String>,
}

/// `start` から親ディレクトリへ遡って最初に見つかった `.env` を返す。
pub fn find(start: &Path) -> Option<PathBuf> {
	start.ancestors().map(|dir| dir.join(FILE_NAME)).find(|p| p.is_file())
}

/// `.env` を読み、環境にまだ無いものを返す。環境変数は書き換えない。
///
/// - `path` が `Some` のときはそのファイルを読む (無ければエラー)。
/// - `None` のときはカレントディレクトリから [`find`] で探し、見つからなければ `Ok(None)`。
pub fn load(path: Option<&Path>) -> Result<Option<Loaded>, Error> {
	let path = match path {
		Some(p) => p.to_path_buf(),
		None => {
			let cwd = std::env::current_dir().map_err(|source| Error::Io {
				path: PathBuf::from("."),
				source,
			})?;
			match find(&cwd) {
				Some(p) => p,
				None => return Ok(None),
			}
		}
	};

	let src = std::fs::read_to_string(&path).map_err(|source| Error::Io {
		path: path.clone(),
		source,
	})?;
	let entries = parse(&src).map_err(|e| Error::Parse {
		path: path.clone(),
		line: e.line,
		message: e.message,
	})?;

	let mut loaded = Loaded {
		path,
		..Default::default()
	};
	for (key, value) in entries {
		if std::env::var_os(&key).is_some() {
			loaded.skipped.push(key);
		} else {
			loaded.env.push((key, value));
		}
	}
	Ok(Some(loaded))
}

/// `.env` の中身をパースして `(KEY, VALUE)` を出現順に返す。同じキーは後勝ち。
pub fn parse(src: &str) -> Result<Vec<(String, String)>, ParseError> {
	let mut out: Vec<(String, String)> = Vec::new();
	let mut lines = src.lines().enumerate().map(|(i, l)| (i + 1, l));

	while let Some((lineno, raw)) = lines.next() {
		let line = raw.trim_start();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
		let err = |message: &str| ParseError {
			line: lineno,
			message: message.into(),
		};

		let (key, rest) = line
			.split_once('=')
			.ok_or_else(|| err("`KEY=VALUE` の形式ではありません"))?;
		let key = key.trim();
		if !is_valid_key(key) {
			return Err(err(&format!("不正なキー名です: `{key}`")));
		}

		let rest = rest.trim_start();
		let value = match rest.chars().next() {
			Some(q @ ('"' | '\'')) => {
				// 閉じクォートが見つかるまで次の行を連結する (複数行の値)。
				let mut buf = rest[1..].to_string();
				let (body, tail) = loop {
					if let Some(end) = find_closing(&buf, q) {
						break (buf[..end].to_string(), buf[end + 1..].to_string());
					}
					match lines.next() {
						Some((_, next)) => {
							buf.push('\n');
							buf.push_str(next);
						}
						None => return Err(err("クォートが閉じられていません")),
					}
				};
				let tail = tail.trim();
				if !tail.is_empty() && !tail.starts_with('#') {
					return Err(err("閉じクォートの後に余分な文字があります"));
				}
				if q == '"' { unescape(&body) } else { body }
			}
			_ => strip_inline_comment(rest).trim_end().to_string(),
		};

		match out.iter_mut().find(|(k, _)| k == key) {
			Some(slot) => slot.1 = value,
			None => out.push((key.to_string(), value)),
		}
	}
	Ok(out)
}

fn is_valid_key(key: &str) -> bool {
	let mut chars = key.chars();
	matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
		&& chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// クォート内の本文 `s` から閉じクォートの位置を探す。`"` ではバックスラッシュエスケープを考慮する。
fn find_closing(s: &str, quote: char) -> Option<usize> {
	let mut escaped = false;
	for (i, c) in s.char_indices() {
		if quote == '"' && escaped {
			escaped = false;
		} else if quote == '"' && c == '\\' {
			escaped = true;
		} else if c == quote {
			return Some(i);
		}
	}
	None
}

fn unescape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut chars = s.chars();
	while let Some(c) = chars.next() {
		if c != '\\' {
			out.push(c);
			continue;
		}
		match chars.next() {
			Some('n') => out.push('\n'),
			Some('r') => out.push('\r'),
			Some('t') => out.push('\t'),
			Some(other) => out.push(other),
			None => out.push('\\'),
		}
	}
	out
}

/// クォートなしの値から ` #...` 以降 (空白の後の `#`) を落とす。`a#b` はそのまま。
fn strip_inline_comment(s: &str) -> &str {
	let mut prev_ws = true;
	for (i, c) in s.char_indices() {
		if c == '#' && prev_ws {
			return &s[..i];
		}
		prev_ws = c.is_whitespace();
	}
	s
}

#[cfg(test)]
mod tests {
	use super::*;

	fn opts(items: &[(&str, &str)]) -> Value {
		Value::Object(
			items
				.iter()
				.map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
				.collect(),
		)
	}

	#[test]
	fn boot_returns_env_without_touching_the_process() {
		let dir = std::env::temp_dir().join(format!("lh-dotenv-boot-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let file = dir.join(FILE_NAME);
		std::fs::write(&file, "LH_DOTENV_BOOT_KEY=from_file\n").unwrap();

		let env = DotenvPlugin
			.boot(&opts(&[("env_file", file.to_str().unwrap())]))
			.unwrap();
		assert_eq!(env.get("LH_DOTENV_BOOT_KEY").map(String::as_str), Some("from_file"));
		assert!(std::env::var_os("LH_DOTENV_BOOT_KEY").is_none());
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn boot_can_be_disabled_and_rejects_conflicts() {
		assert!(DotenvPlugin.boot(&opts(&[("no_env_file", "true")])).unwrap().is_empty());
		let err = DotenvPlugin
			.boot(&opts(&[("env_file", "x.env"), ("no_env_file", "true")]))
			.unwrap_err();
		assert!(matches!(err, letsharness_kernel::Error::Config(_)));
	}

	fn kv(src: &str) -> Vec<(String, String)> {
		parse(src).unwrap()
	}

	fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
		items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
	}

	#[test]
	fn basic_and_comments() {
		let src = "\
# comment
FOO=bar

export BAZ = qux
EMPTY=
URL=http://example.com/#frag # trailing comment
";
		assert_eq!(
			kv(src),
			pairs(&[
				("FOO", "bar"),
				("BAZ", "qux"),
				("EMPTY", ""),
				("URL", "http://example.com/#frag"),
			])
		);
	}

	#[test]
	fn quoted_values() {
		let src = r#"
A="hello world" # c
B='raw \n # not comment'
C="line1\nline2\t\"q\" \\"
D="multi
line"
E='x'
"#;
		assert_eq!(
			kv(src),
			pairs(&[
				("A", "hello world"),
				("B", r"raw \n # not comment"),
				("C", "line1\nline2\t\"q\" \\"),
				("D", "multi\nline"),
				("E", "x"),
			])
		);
	}

	#[test]
	fn last_wins() {
		assert_eq!(kv("K=1\nK=2\n"), pairs(&[("K", "2")]));
	}

	#[test]
	fn errors_have_line_numbers() {
		assert_eq!(parse("A=1\nnot a pair\n").unwrap_err().line, 2);
		assert_eq!(parse("\n1BAD=x").unwrap_err().line, 2);
		assert_eq!(parse("A=\"open\nstill open").unwrap_err().line, 1);
		assert_eq!(parse("A=\"x\" junk").unwrap_err().line, 1);
	}

	#[test]
	fn find_walks_up() {
		let root = std::env::temp_dir().join(format!("lh-dotenv-find-{}", std::process::id()));
		let nested = root.join("a/b");
		std::fs::create_dir_all(&nested).unwrap();
		std::fs::write(root.join(FILE_NAME), "X=1\n").unwrap();

		assert_eq!(find(&nested), Some(root.join(FILE_NAME)));
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn load_skips_variables_already_in_the_environment() {
		let dir = std::env::temp_dir().join(format!("lh-dotenv-load-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let file = dir.join(FILE_NAME);
		// PATH は必ず環境にある。
		std::fs::write(&file, "LH_DOTENV_TEST_NEW=from_file\nPATH=from_file\n").unwrap();

		let loaded = load(Some(&file)).unwrap().unwrap();

		assert_eq!(loaded.env, pairs(&[("LH_DOTENV_TEST_NEW", "from_file")]));
		assert_eq!(loaded.skipped, vec!["PATH"]);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn load_missing_explicit_path_is_error() {
		let missing = std::env::temp_dir().join("lh-dotenv-definitely-missing/.env");
		assert!(matches!(load(Some(&missing)), Err(Error::Io { .. })));
	}
}
