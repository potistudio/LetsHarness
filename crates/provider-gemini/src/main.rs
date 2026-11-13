//! kernel から起動されるプラグインプロセス。
#[tokio::main]
async fn main() {
	letsharness_provider_gemini::server().serve().await;
}
