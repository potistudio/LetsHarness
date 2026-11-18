//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_observer_log::server().serve().await;
}
