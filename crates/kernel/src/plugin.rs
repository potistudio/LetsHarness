//! プラグインの入口 (`Plugin`) と、ランタイム起動前に走る `early_init` フェーズ。
//!
//! `.env` の読み込みのように「環境変数を書き換える」「非同期ランタイムより前に済ませたい」
//! 処理のために、`Plugin` は同期の `early_init` を持つ。
//! プラグインは自分のフラグだけを [`BootArgs`] から抜き取り、残りは Frontend (clap など) へそのまま渡る。
//! kernel は `clap` に依存しないので、ここでは `--name` / `--name VALUE` / `--name=VALUE` だけを扱う。

use std::ffi::{OsStr, OsString};

use crate::{Error, Registry, Result};

/// すべてのプラグインの入口。
pub trait Plugin: Send + Sync {
	/// Name of the plugin.
	fn name(&self) -> &'static str;

	/// ランタイム起動前に一度だけ呼ばれる。自分のフラグを `args` から抜き取って処理する。
	///
	/// [`run_early_init`] 経由で呼ばれる間は、他のスレッドが存在しない
	/// (= 環境変数を安全に書き換えられる) ことが保証される。
	fn early_init(&self, _args: &mut BootArgs) -> Result<()> {
		Ok(())
	}

	/// `early_init` が抜き取るフラグの説明。Frontend の `--help` に添えられる。
	fn boot_help(&self) -> &'static str {
		""
	}

	/// 拡張点 (Provider / Tool / Frontend ...) を `registry` に登録する。
	fn register(&self, _registry: &mut Registry) -> Result<()> {
		Ok(())
	}
}

/// 全プラグインの [`Plugin::early_init`] を登録順に呼ぶ。
///
/// # Safety
///
/// プラグインは `early_init` の中で [`std::env::set_var`] を呼んでよい。
/// 呼び出し側は、他のスレッドが環境変数を読み書きしていない
/// (= 非同期ランタイムやスレッドを起動する前の) 状態で呼ぶこと。
pub unsafe fn run_early_init(plugins: &[Box<dyn Plugin>], args: &mut BootArgs) -> Result<()> {
	for plugin in plugins {
		plugin.early_init(args)?;
	}

	Ok(())
}

/// 起動引数。プラグインが自分のフラグを抜き取り、残りを Frontend に渡す。
///
/// 先頭 (プログラム名) と、`--` 以降は触らない。
#[derive(Debug, Clone)]
pub struct BootArgs {
	args: Vec<OsString>,
}

impl BootArgs {
	/// `args[0]` はプログラム名として扱う。
	pub fn new(args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
		Self {
			args: args.into_iter().map(Into::into).collect(),
		}
	}

	/// 現在のプロセスの引数から作る。
	pub fn from_env() -> Self {
		Self::new(std::env::args_os())
	}

	/// 抜き取られずに残った引数 (プログラム名を含む)。
	pub fn into_args(self) -> Vec<OsString> {
		self.args
	}

	/// `--name` を取り除き、あったかどうかを返す。
	pub fn take_flag(&mut self, name: &str) -> bool {
		let before = self.args.len();
		let mut end = self.options_end();
		let mut i = 1;
		while i < end {
			if self.args[i] == name {
				self.args.remove(i);
				end -= 1;
			} else {
				i += 1;
			}
		}
		self.args.len() != before
	}

	/// `--name VALUE` または `--name=VALUE` を取り除き、最後に指定された値を返す。
	///
	/// 値が無い (`--name` が末尾、または次が別のフラグ) ときは `Error::Config`。
	pub fn take_value(&mut self, name: &str) -> Result<Option<OsString>> {
		let prefix = format!("{name}=");
		let mut found = None;
		let mut end = self.options_end();
		let mut i = 1;
		while i < end {
			if self.args[i] == name {
				let has_value = i + 1 < end && !is_flag(&self.args[i + 1]);
				if !has_value {
					return Err(Error::Config(format!("{name} には値が必要です")));
				}
				found = Some(self.args.remove(i + 1));
				self.args.remove(i);
				end -= 2;
			} else if let Some(value) = strip_prefix(&self.args[i], &prefix) {
				found = Some(value);
				self.args.remove(i);
				end -= 1;
			} else {
				i += 1;
			}
		}
		Ok(found)
	}

	/// 先頭の `--` の位置 (無ければ末尾)。
	fn options_end(&self) -> usize {
		self.args.iter().position(|a| a == "--").unwrap_or(self.args.len())
	}
}

fn is_flag(arg: &OsStr) -> bool {
	arg.as_encoded_bytes().starts_with(b"-")
}

/// `arg` が `prefix` (ASCII) で始まるとき、残りを返す。
fn strip_prefix(arg: &OsStr, prefix: &str) -> Option<OsString> {
	let rest = arg.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
	// SAFETY: `rest` は `as_encoded_bytes` の結果を ASCII 部分の直後で切ったもの。
	// `OsStr::from_encoded_bytes_unchecked` の要件 (有効な部分列) を満たす。
	Some(unsafe { OsStr::from_encoded_bytes_unchecked(rest) }.to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn boot(args: &[&str]) -> BootArgs {
		BootArgs::new(args.iter().copied())
	}

	fn rest(args: BootArgs) -> Vec<String> {
		args.into_args()
			.into_iter()
			.map(|a| a.to_string_lossy().into_owned())
			.collect()
	}

	#[test]
	fn take_flag_removes_all_occurrences() {
		let mut args = boot(&["prog", "--x", "chat", "--x", "--y"]);
		assert!(args.take_flag("--x"));
		assert!(!args.take_flag("--z"));
		assert_eq!(rest(args), ["prog", "chat", "--y"]);
	}

	#[test]
	fn take_value_supports_both_forms_and_last_wins() {
		let mut args = boot(&["prog", "--f", "a", "chat", "--f=b"]);
		assert_eq!(args.take_value("--f").unwrap(), Some(OsString::from("b")));
		assert_eq!(rest(args), ["prog", "chat"]);
	}

	#[test]
	fn take_value_missing_value_is_config_error() {
		let mut tail = boot(&["prog", "--f"]);
		assert!(matches!(tail.take_value("--f"), Err(Error::Config(_))));
		let mut next_flag = boot(&["prog", "--f", "--g"]);
		assert!(matches!(next_flag.take_value("--f"), Err(Error::Config(_))));
	}

	#[test]
	fn take_value_absent_is_none() {
		let mut args = boot(&["prog", "chat"]);
		assert_eq!(args.take_value("--f").unwrap(), None);
		assert_eq!(rest(args), ["prog", "chat"]);
	}

	#[test]
	fn args_after_double_dash_and_program_name_are_untouched() {
		let mut args = boot(&["--x", "a", "--", "--x", "--f=v"]);
		assert!(!args.take_flag("--x"));
		assert_eq!(args.take_value("--f").unwrap(), None);
		assert_eq!(rest(args), ["--x", "a", "--", "--x", "--f=v"]);
	}

	#[test]
	fn run_early_init_runs_in_order_and_stops_on_error() {
		use std::sync::Mutex;

		static LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

		struct P(&'static str, bool);
		impl Plugin for P {
			fn name(&self) -> &'static str {
				self.0
			}
			fn early_init(&self, _args: &mut BootArgs) -> Result<()> {
				LOG.lock().unwrap().push(self.0);
				if self.1 {
					Err(Error::Config("boom".into()))
				} else {
					Ok(())
				}
			}
		}

		let plugins: Vec<Box<dyn Plugin>> =
			vec![Box::new(P("a", false)), Box::new(P("b", true)), Box::new(P("c", false))];
		let mut args = boot(&["prog"]);
		// SAFETY: テストは環境変数に触らない。
		let result = unsafe { run_early_init(&plugins, &mut args) };
		assert!(result.is_err());
		assert_eq!(*LOG.lock().unwrap(), ["a", "b"]);
	}
}
