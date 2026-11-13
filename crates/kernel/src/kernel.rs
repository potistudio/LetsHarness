//! プラグインを束ねて起動する `Kernel`。
//!
//! プラグインを使うのは kernel。Client (Frontend) を含むすべてのプラグインは、
//! kernel に渡された `Plugin` として `early_init` → `register` → Frontend 起動の順に扱われる。
//! bin (`main.rs`) は「どのプラグインを渡すか」を書くだけで、プラグインの中身は知らない。

use std::process::ExitCode;
use std::sync::Arc;

use crate::{BootArgs, Error, Frontend, Host, Plugin, Registry, Result, run_early_init};

#[derive(Default)]
pub struct Kernel {
	plugins: Vec<Box<dyn Plugin>>,
}

impl Kernel {
	pub fn new() -> Self {
		Self::default()
	}

	/// プラグインを足す。`early_init` / `register` はこの順に呼ばれる。
	pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
		self.plugins.push(Box::new(plugin));
		self
	}

	/// プロセスの引数で起動する。`--frontend <name>` を抜き取り、無ければ最初の Frontend を使う。
	///
	/// # Safety
	///
	/// プラグインは `early_init` で環境変数を書き換えうる。
	/// 他のスレッドが存在しない (= `main` の先頭) 状態で呼ぶこと。
	pub unsafe fn run(self) -> ExitCode {
		// SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
		unsafe { self.run_with(BootArgs::from_env()) }
	}

	/// [`Kernel::run`] の引数指定版。
	///
	/// # Safety
	///
	/// [`Kernel::run`] と同じ。
	pub unsafe fn run_with(self, mut boot: BootArgs) -> ExitCode {
		// SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
		match unsafe { self.start(&mut boot) } {
			Ok((host, frontend)) => frontend.run(&host, boot.into_args()),
			Err(e) => {
				eprintln!("Error: {e}");
				ExitCode::FAILURE
			}
		}
	}

	unsafe fn start(&self, boot: &mut BootArgs) -> Result<(Host, Arc<dyn Frontend>)> {
		// SAFETY: 呼び出し側の契約。
		unsafe { run_early_init(&self.plugins, boot) }?;

		let wanted = boot.take_value("--frontend")?;

		let mut registry = Registry::new();
		for plugin in &self.plugins {
			plugin.register(&mut registry)?;
		}

		let frontends = registry.all::<dyn Frontend>();
		let frontend = match &wanted {
			Some(name) => {
				let name = name.to_string_lossy();
				frontends.iter().find(|f| f.name() == name).cloned().ok_or_else(|| {
					let known: Vec<_> = frontends.iter().map(|f| f.name()).collect();
					Error::Config(format!("unknown frontend: {name} ({})", known.join("|")))
				})?
			}
			None => frontends
				.first()
				.cloned()
				.ok_or_else(|| Error::Config("Frontend プラグインが登録されていません".into()))?,
		};
		let help: Vec<_> = self
			.plugins
			.iter()
			.map(|p| p.boot_help())
			.filter(|h| !h.is_empty())
			.collect();
		Ok((Host::new(registry).with_boot_help(help.join("\n")), frontend))
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsString;
	use std::sync::Mutex;

	use super::*;

	static SEEN: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());

	struct Fe(&'static str);
	impl Frontend for Fe {
		fn name(&self) -> &'static str {
			self.0
		}
		fn run(&self, _host: &Host, args: Vec<OsString>) -> ExitCode {
			let args = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
			SEEN.lock().unwrap().push((self.0.into(), args));
			ExitCode::SUCCESS
		}
	}

	struct FePlugin(&'static str);
	impl Plugin for FePlugin {
		fn name(&self) -> &'static str {
			self.0
		}
		fn register(&self, registry: &mut Registry) -> Result<()> {
			registry.add::<dyn Frontend>(Arc::new(Fe(self.0)));
			Ok(())
		}
	}

	fn run(kernel: Kernel, args: &[&str]) {
		// SAFETY: テストは環境変数に触らない。
		unsafe { kernel.run_with(BootArgs::new(args.iter().copied())) };
	}

	#[test]
	fn kernel_picks_frontend_and_passes_remaining_args() {
		let mk = || Kernel::new().plugin(FePlugin("first-fe")).plugin(FePlugin("second-fe"));

		run(mk(), &["prog", "chat"]);
		run(mk(), &["prog", "--frontend=second-fe", "chat"]);
		run(mk(), &["prog", "--frontend", "nope"]);

		let seen = SEEN.lock().unwrap();
		assert_eq!(seen.len(), 2, "unknown frontend must not run");
		assert_eq!(seen[0], ("first-fe".into(), vec!["prog".into(), "chat".into()]));
		assert_eq!(seen[1], ("second-fe".into(), vec!["prog".into(), "chat".into()]));
	}
}
