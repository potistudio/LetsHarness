//! 各ツールが共有する小さな部品。

use letsharness_kernel::{ToolOutput, ToolSpec};
use serde::Deserialize;

/// 引数 JSON を型に落とす。失敗は「引数が不正」としてモデルへ返す。
pub(crate) fn parse_args<T: for<'de> Deserialize<'de>>(args: serde_json::Value) -> Result<T, ToolOutput> {
	serde_json::from_value(args).map_err(|e| ToolOutput::error(format!("invalid arguments: {e}")))
}

pub(crate) fn spec(
	name: &str,
	description: &str,
	properties: serde_json::Value,
	required: &[&str],
	approval: bool,
) -> ToolSpec {
	ToolSpec {
		name: name.into(),
		description: description.into(),
		input_schema: serde_json::json!({
			"type": "object",
			"properties": properties,
			"required": required,
		}),
		requires_approval: approval,
	}
}
