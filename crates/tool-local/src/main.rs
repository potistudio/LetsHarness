//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_tool_local::server().serve().await;
}
