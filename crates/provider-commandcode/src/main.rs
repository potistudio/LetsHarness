//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_provider_commandcode::server().serve().await;
}
