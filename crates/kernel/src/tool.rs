//! ツール拡張点: モデルが呼び出せる関数の定義と登録。
//!
//! スキーマは MCP / 各社 function calling と同形 (`name` / `description` / JSON Schema)。
//! 型はプロセス境界を越えられるよう、独自拡張は `serde_json::Value` のみにする。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Tool definition.
///
/// This is passed to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
	/// The name of the tool.
	///
	/// This is used to identify the tool when the models is calling it.
	pub name: String,

	/// The description of the tool.
	///
	/// This is used to describe the tool to the model.
	/// The model may use this description to decide whether to call the tool or not.
	/// The description should be concise and clear.
	pub description: String,

	/// 引数の JSON Schema (`{"type":"object","properties":{...}}`)。
	pub input_schema: serde_json::Value,

	/// `true` なら実行前に [`RunHandler::approve`](crate::RunHandler::approve) で承認を取る。
	/// 副作用のあるツール (書き込み・コマンド実行など) は立てる。
	#[serde(default)]
	pub requires_approval: bool,
}

/// モデルが要求した1回のツール呼び出し。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
	/// 呼び出しID。結果メッセージ (`tool_call_id`) との対応づけに使う。
	pub id: String,
	pub name: String,
	pub arguments: serde_json::Value,
	/// プロバイダ固有の付帯情報 (例: Gemini の `thoughtSignature`)。
	/// 履歴として同じプロバイダへ送り返すときに使う。他のプロバイダは無視する。
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub extra: serde_json::Value,
}

impl ToolCall {
	pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: serde_json::Value) -> Self {
		Self {
			id: id.into(),
			name: name.into(),
			arguments,
			extra: serde_json::Value::Null,
		}
	}
}

/// ツールの実行結果。モデルへはそのままテキストとして返る。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
	pub content: String,
	#[serde(default)]
	pub is_error: bool,
}

impl ToolOutput {
	pub fn ok(content: impl Into<String>) -> Self {
		Self {
			content: content.into(),
			is_error: false,
		}
	}

	/// 失敗もエラーで止めず、モデルに見せて自己修正させる。
	pub fn error(content: impl Into<String>) -> Self {
		Self {
			content: content.into(),
			is_error: true,
		}
	}
}

/// エージェントが呼ぶツール。
///
/// `call` は失敗を [`ToolOutput::error`] で返す (`Result` にしない)。
/// ツールの失敗はハーネスの失敗ではなく、モデルへ渡す情報だから。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
	fn spec(&self) -> ToolSpec;
	async fn call(&self, args: serde_json::Value) -> ToolOutput;
}

/// 名前でツールを引く入れ物。登録順を保つ。
#[derive(Clone, Default)]
pub struct ToolRegistry {
	tools: Vec<(ToolSpec, Arc<dyn Tool>)>,
}

impl ToolRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	/// 登録する。同名があれば置き換える。
	pub fn add(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
		let spec = tool.spec();
		match self.tools.iter_mut().find(|(s, _)| s.name == spec.name) {
			Some(slot) => *slot = (spec, tool),
			None => self.tools.push((spec, tool)),
		}
		self
	}

	pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
		self.add(tool);
		self
	}

	pub fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> &mut Self {
		for t in tools {
			self.add(t);
		}
		self
	}

	pub fn get(&self, name: &str) -> Option<(&ToolSpec, &Arc<dyn Tool>)> {
		self.tools.iter().find(|(s, _)| s.name == name).map(|(s, t)| (s, t))
	}

	pub fn specs(&self) -> Vec<ToolSpec> {
		self.tools.iter().map(|(s, _)| s.clone()).collect()
	}

	pub fn is_empty(&self) -> bool {
		self.tools.is_empty()
	}

	pub fn len(&self) -> usize {
		self.tools.len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Named(&'static str, &'static str);

	#[async_trait::async_trait]
	impl Tool for Named {
		fn spec(&self) -> ToolSpec {
			ToolSpec {
				name: self.0.into(),
				description: self.1.into(),
				input_schema: serde_json::json!({"type": "object"}),
				requires_approval: false,
			}
		}
		async fn call(&self, _args: serde_json::Value) -> ToolOutput {
			ToolOutput::ok(self.1)
		}
	}

	#[test]
	fn registry_keeps_order_and_replaces_same_name() {
		let mut r = ToolRegistry::new();
		r.add(Arc::new(Named("a", "first")));
		r.add(Arc::new(Named("b", "b")));
		r.add(Arc::new(Named("a", "second")));
		let specs = r.specs();
		assert_eq!(specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
		assert_eq!(r.get("a").unwrap().0.description, "second");
		assert!(r.get("zzz").is_none());
		assert_eq!(r.len(), 2);
	}
}
