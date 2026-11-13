//! 同梱プラグインを kernel に渡すだけの composition root。ロジックは持たない。

use std::process::ExitCode;

use letsharness_agent_antigravity::AntigravityPlugin;
use letsharness_frontend_cli::CliPlugin;
use letsharness_kernel::Kernel;
use letsharness_plugin_dotenv::DotenvPlugin;
use letsharness_provider_commandcode::CommandCodePlugin;
use letsharness_provider_gemini::GeminiPlugin;
use letsharness_provider_mock::MockPlugin;
use letsharness_tool_builtin::BuiltinToolsPlugin;

fn main() -> ExitCode {
	let kernel = Kernel::new()
		.plugin(DotenvPlugin)
		.plugin(CommandCodePlugin)
		.plugin(GeminiPlugin)
		.plugin(MockPlugin)
		.plugin(AntigravityPlugin)
		.plugin(BuiltinToolsPlugin)
		.plugin(CliPlugin);

	// SAFETY: プロセスの先頭で、まだ他のスレッドを起動していない。
	unsafe { kernel.run() }
}
