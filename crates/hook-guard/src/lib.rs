//! `deny_tools` に挙げたツールを実行させない Hook。
//!
//! `-o deny_tools=bash,write_file` (カンマ区切り、または JSON 配列) で指定する。
//! 止めた呼び出しはエラー結果としてモデルに返るので、モデルは別の手段を選べる。
//! 指定が無ければどこにも割り込まない。

use std::sync::Arc;

use letsharness_kernel::{Hook, HookFactory, HookPoint, PluginServer, Result, ToolCall, ToolDecision, ToolOutput};
use serde_json::Value;

pub fn server() -> PluginServer {
	PluginServer::new().hook(GuardFactory)
}

struct GuardFactory;

impl HookFactory for GuardFactory {
	fn name(&self) -> &'static str {
		"guard"
	}

	fn create(&self, cfg: &Value) -> Result<Arc<dyn Hook>> {
		Ok(Arc::new(Guard {
			deny: deny_list(cfg.get("deny_tools")),
		}))
	}
}

fn deny_list(v: Option<&Value>) -> Vec<String> {
	let names: Vec<&str> = match v {
		Some(Value::String(s)) => s.split(',').collect(),
		Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
		_ => Vec::new(),
	};
	names
		.into_iter()
		.map(str::trim)
		.filter(|n| !n.is_empty())
		.map(str::to_owned)
		.collect()
}

struct Guard {
	deny: Vec<String>,
}

#[async_trait::async_trait]
impl Hook for Guard {
	fn points(&self) -> Vec<HookPoint> {
		if self.deny.is_empty() {
			Vec::new()
		} else {
			vec![HookPoint::OnToolCall]
		}
	}

	async fn on_tool_call(&self, call: ToolCall) -> Result<ToolDecision> {
		if self.deny.contains(&call.name) {
			return Ok(ToolDecision::Skip {
				output: ToolOutput::error(format!("tool `{}` is blocked by deny_tools", call.name)),
			});
		}
		Ok(ToolDecision::Run { call })
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[tokio::test]
	async fn blocks_only_denied_tools() {
		let guard = GuardFactory
			.create(&json!({ "deny_tools": "bash, write_file" }))
			.unwrap();
		assert_eq!(guard.points(), [HookPoint::OnToolCall]);

		let denied = guard
			.on_tool_call(ToolCall::new("c1", "bash", json!({})))
			.await
			.unwrap();
		assert!(matches!(denied, ToolDecision::Skip { output } if output.is_error));

		let allowed = guard
			.on_tool_call(ToolCall::new("c2", "read_file", json!({})))
			.await
			.unwrap();
		assert!(matches!(allowed, ToolDecision::Run { call } if call.name == "read_file"));
	}

	#[test]
	fn idle_without_deny_tools() {
		assert!(GuardFactory.create(&json!({})).unwrap().points().is_empty());
		let from_array = GuardFactory.create(&json!({ "deny_tools": ["bash"] })).unwrap();
		assert_eq!(from_array.points(), [HookPoint::OnToolCall]);
	}
}
