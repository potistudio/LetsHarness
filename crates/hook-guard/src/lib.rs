//! `deny_tools` に挙げたツールを実行させない Hook。
//!
//! `-o deny_tools=bash,write_file` (カンマ区切り、または JSON 配列) で指定する。
//! 止めた呼び出しはエラー結果としてモデルに返るので、モデルは別の手段を選べる。
//! 指定が無ければどこにも割り込まない。存在しないツール名を指定すると、打ち間違いを見逃さないよう起動を止める。

use std::sync::Arc;

use letsharness_kernel::{
	Error, Hook, HookFactory, HookPoint, PluginServer, Result, ToolCall, ToolDecision, ToolOutput, ToolSpec,
};
use serde_json::Value;

pub fn server() -> PluginServer {
	PluginServer::new().hook(GuardFactory)
}

struct GuardFactory;

impl HookFactory for GuardFactory {
	fn name(&self) -> &'static str {
		"guard"
	}

	fn create(&self, cfg: &Value, tools: &[ToolSpec]) -> Result<Arc<dyn Hook>> {
		let deny = deny_list(cfg.get("deny_tools"));
		if let Some(unknown) = deny.iter().find(|n| !tools.iter().any(|t| &t.name == *n)) {
			let known: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
			return Err(Error::Config(format!(
				"deny_tools: unknown tool `{unknown}` ({})",
				known.join("|")
			)));
		}
		Ok(Arc::new(Guard { deny }))
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

	fn tools(names: &[&str]) -> Vec<ToolSpec> {
		names
			.iter()
			.map(|n| ToolSpec {
				name: (*n).into(),
				description: String::new(),
				input_schema: json!({"type": "object"}),
			})
			.collect()
	}

	#[tokio::test]
	async fn blocks_only_denied_tools() {
		let guard = GuardFactory
			.create(
				&json!({ "deny_tools": "bash, write_file" }),
				&tools(&["bash", "write_file", "read_file"]),
			)
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
		assert!(GuardFactory.create(&json!({}), &[]).unwrap().points().is_empty());
		let from_array = GuardFactory
			.create(&json!({ "deny_tools": ["bash"] }), &tools(&["bash"]))
			.unwrap();
		assert_eq!(from_array.points(), [HookPoint::OnToolCall]);
	}

	#[test]
	fn unknown_tool_names_fail_loud() {
		let err = GuardFactory
			.create(&json!({ "deny_tools": "bsah" }), &tools(&["bash"]))
			.err()
			.expect("typo must be rejected");
		assert_eq!(err.to_string(), "config: deny_tools: unknown tool `bsah` (bash)");
	}
}
