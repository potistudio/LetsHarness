//! ループの出来事を JSON Lines で追記する Observer。
//!
//! `-o event_log=PATH` で有効になり、1行に1つの [`Event`] を書く。指定が無ければ何も見ない。
//! 相対パスは Kernel のカレントディレクトリから解決する。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

use letsharness_kernel::{Error, Event, Observer, ObserverFactory, PluginServer, Result};
use serde_json::Value;

pub fn server() -> PluginServer {
	PluginServer::new().observer(EventLogFactory)
}

struct EventLogFactory;

impl ObserverFactory for EventLogFactory {
	fn name(&self) -> &'static str {
		"event-log"
	}

	fn create(&self, cfg: &Value) -> Result<Option<Arc<dyn Observer>>> {
		let Some(path) = cfg.get("event_log").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
			return Ok(None);
		};
		let file = OpenOptions::new()
			.create(true)
			.append(true)
			.open(path)
			.map_err(|e| Error::Config(format!("event_log: {path}: {e}")))?;
		Ok(Some(Arc::new(EventLog { file: Mutex::new(file) })))
	}
}

struct EventLog {
	file: Mutex<File>,
}

#[async_trait::async_trait]
impl Observer for EventLog {
	fn on_event(&self, event: &Event) {
		let Ok(line) = serde_json::to_string(event) else { return };
		let mut file = self.file.lock().unwrap();
		if let Err(e) = writeln!(file, "{line}") {
			eprintln!("warning: event_log: {e}");
		}
	}

	async fn flush(&self) {
		if let Err(e) = self.file.lock().unwrap().flush() {
			eprintln!("warning: event_log: {e}");
		}
	}
}

#[cfg(test)]
mod tests {
	use letsharness_kernel::{ToolCall, ToolOutput};
	use serde_json::json;

	use super::*;

	#[tokio::test]
	async fn appends_one_event_per_line() {
		let path = std::env::temp_dir().join(format!("lh-event-log-{}.jsonl", std::process::id()));
		let _ = std::fs::remove_file(&path);
		let observer = EventLogFactory
			.create(&json!({ "event_log": path }))
			.unwrap()
			.expect("enabled");
		let event = Event::ToolResult {
			call: ToolCall::new("c1", "echo", json!({})),
			output: ToolOutput::ok("hi"),
		};
		observer.on_event(&event);
		observer.on_event(&event);
		observer.flush().await;

		let text = std::fs::read_to_string(&path).unwrap();
		let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
		assert_eq!(lines.len(), 2);
		assert_eq!(lines[0]["event"], "tool_result");
		std::fs::remove_file(&path).unwrap();
	}

	#[test]
	fn idle_without_a_path() {
		assert!(EventLogFactory.create(&json!({})).unwrap().is_none());
	}
}
