//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_agent_loop::server().serve().await;
}
