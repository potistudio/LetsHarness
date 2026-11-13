//! kernel から起動されるプラグインプロセス。
use letsharness_kernel::PluginServer;
use letsharness_plugin_dotenv::DotenvPlugin;

#[tokio::main]
async fn main() {
	PluginServer::new().boot_hook(DotenvPlugin).serve().await;
}
