//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_hook_guard::server().serve().await;
}
