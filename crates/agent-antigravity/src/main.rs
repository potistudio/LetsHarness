//! kernel から起動されるプラグインプロセス。

#[tokio::main]
async fn main() {
	letsharness_agent_antigravity::server().serve().await;
}
