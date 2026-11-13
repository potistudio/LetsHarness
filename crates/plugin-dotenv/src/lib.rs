//! `.env` ファイルを読み込んでプロセスの環境変数に流し込むプラグイン。
//!
//! Provider は `COMMAND_CODE_API_KEY` / `GEMINI_API_KEY` などを環境変数から読むため、
//! それより前 (ランタイム起動前) に [`load`] を呼ぶ。
//!
//! - 既に設定されている環境変数は上書きしない (`環境変数 > .env`)。
//! - 対応する書式: `KEY=VALUE` / `export KEY=VALUE` / `#` コメント /
//!   `"..."` (エスケープ `\n \r \t \" \\` と複数行) / `'...'` (リテラル、複数行) /
//!   クォートなし値の行末コメント (`KEY=v # comment`)。
//! - `${VAR}` の展開はしない。
//!
//! kernel の [`Plugin`] として `early_init` フェーズで動き、`--env-file <PATH>` /
//! `--no-env-file` を起動引数から抜き取る (Frontend はこのプラグインを知らなくてよい)。

use std::path::{Path, PathBuf};

use letsharness_kernel::{BootArgs, Plugin};

/// 既定のファイル名。
pub const FILE_NAME: &str = ".env";

/// `.env` を読み込む kernel プラグイン。
#[derive(Debug, Default, Clone, Copy)]
pub struct DotenvPlugin;

impl Plugin for DotenvPlugin {
	fn name(&self) -> &'static str {
		"dotenv"
	}

	fn boot_help(&self) -> &'static str {
		"Plugin options (dotenv):\n  --env-file <PATH>  読み込む `.env` ファイル。省略時はカレントから親へ遡って探す (環境変数 > .env)\n  --no-env-file      `.env` を読み込まない"
	}

	fn early_init(&self, args: &mut BootArgs) -> letsharness_kernel::Result<()> {
		let no_env_file = args.take_flag("--no-env-file");
		let env_file = args.take_value("--env-file")?;
		if no_env_file {
			if env_file.is_some() {
				return Err(letsharness_kernel::Error::Config(
					"--env-file と --no-env-file は同時に指定できません".into(),
				));
			}
			return Ok(());
		}
		// SAFETY: `early_init` は kernel の `run_early_init` 経由で、ランタイム起動前に呼ばれる (その Safety 契約)。
		unsafe { load(env_file.as_deref().map(Path::new)) }
			.map_err(|e| letsharness_kernel::Error::plugin(self.name(), e))?;
		Ok(())
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
	/// 環境変数に設定したキー。
	pub applied: Vec<String>,
	/// 既に環境変数があったため無視したキー。
	pub skipped: Vec<String>,
}

/// `start` から親ディレクトリへ遡って最初に見つかった `.env` を返す。
pub fn find(start: &Path) -> Option<PathBuf> {
	start.ancestors().map(|dir| dir.join(FILE_NAME)).find(|p| p.is_file())
}

/// `.env` を読み込んで環境変数に設定する。
///
/// - `path` が `Some` のときはそのファイルを読む (無ければエラー)。
/// - `None` のときはカレントディレクトリから [`find`] で探し、見つからなければ `Ok(None)`。
///
/// # Safety
///
/// 内部で [`std::env::set_var`] を呼ぶ。他のスレッドが環境変数を読み書きしていない
/// (= 非同期ランタイムやスレッドを起動する前の) 状態で呼ぶこと。
pub unsafe fn load(path: Option<&Path>) -> Result<Option<Loaded>, Error> {
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
			// SAFETY: 呼び出し側が単一スレッドであることを保証する (関数の Safety 参照)。
			unsafe { std::env::set_var(&key, value) };
			loaded.applied.push(key);
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

	#[test]
	fn plugin_consumes_its_flags_and_leaves_the_rest() {
		let mut args = BootArgs::new(["prog", "--no-env-file", "chat", "--prompt", "hi"]);
		DotenvPlugin.early_init(&mut args).unwrap();
		let rest: Vec<_> = args
			.into_args()
			.into_iter()
			.map(|a| a.to_string_lossy().into_owned())
			.collect();
		assert_eq!(rest, ["prog", "chat", "--prompt", "hi"]);
	}

	#[test]
	fn plugin_rejects_conflicting_flags() {
		let mut args = BootArgs::new(["prog", "--env-file", "x.env", "--no-env-file"]);
		let err = DotenvPlugin.early_init(&mut args).unwrap_err();
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
	fn load_does_not_override_existing() {
		let dir = std::env::temp_dir().join(format!("lh-dotenv-load-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let file = dir.join(FILE_NAME);
		std::fs::write(
			&file,
			"LH_DOTENV_TEST_NEW=from_file\nLH_DOTENV_TEST_EXISTING=from_file\n",
		)
		.unwrap();

		// SAFETY: このテスト専用のキーのみを触る。
		unsafe { std::env::set_var("LH_DOTENV_TEST_EXISTING", "from_env") };
		let loaded = unsafe { load(Some(&file)) }.unwrap().unwrap();

		assert_eq!(loaded.applied, vec!["LH_DOTENV_TEST_NEW"]);
		assert_eq!(loaded.skipped, vec!["LH_DOTENV_TEST_EXISTING"]);
		assert_eq!(std::env::var("LH_DOTENV_TEST_NEW").unwrap(), "from_file");
		assert_eq!(std::env::var("LH_DOTENV_TEST_EXISTING").unwrap(), "from_env");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn load_missing_explicit_path_is_error() {
		let missing = std::env::temp_dir().join("lh-dotenv-definitely-missing/.env");
		assert!(matches!(unsafe { load(Some(&missing)) }, Err(Error::Io { .. })));
	}
}
