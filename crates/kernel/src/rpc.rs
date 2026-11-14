//! プラグインとの線の上のメッセージ (stdio, 1行1メッセージの JSON-RPC 風)。
//!
//! - リクエスト : `{"id", "method", "params"}` → 応答 `{"id", "result"}` または `{"id", "error"}`
//! - 通知       : `{"method", "params"}` (応答なし)
//! - ストリームやエージェントの途中経過は、`params.req` に元リクエストの `id` を入れた通知で流す。
//!   プラグインから Kernel への問い合わせは `params.req` を持つリクエストで、Kernel は元リクエストの
//!   処理の中で答える (プロトコル上の予約。現在 `PluginServer` に送る API は無い)。
//! - 結果が `null` だと「無い」と区別できないので、空の結果は `{}` にする。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frame {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub method: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub params: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub result: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<WireError>,
}

impl Frame {
	pub fn request(id: u64, method: &str, params: Value) -> Self {
		Self {
			id: Some(id),
			method: Some(method.into()),
			params: Some(params),
			..Default::default()
		}
	}

	pub fn notify(method: &str, params: Value) -> Self {
		Self {
			method: Some(method.into()),
			params: Some(params),
			..Default::default()
		}
	}

	pub fn ok(id: u64, result: Value) -> Self {
		let result = if result.is_null() {
			Value::Object(Default::default())
		} else {
			result
		};
		Self {
			id: Some(id),
			result: Some(result),
			..Default::default()
		}
	}

	pub fn err(id: u64, error: WireError) -> Self {
		Self {
			id: Some(id),
			error: Some(error),
			..Default::default()
		}
	}

	/// 元リクエストの id (`params.req`)。
	pub fn req(&self) -> Option<u64> {
		self.params.as_ref()?.get("req")?.as_u64()
	}
}

/// 線の上のエラー。kernel の [`Error`] と相互に変換する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
	/// `unsupported` / `config` / `api` / `plugin`
	pub kind: String,
	pub message: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub status: Option<u16>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code: Option<String>,
}

impl From<&Error> for WireError {
	fn from(e: &Error) -> Self {
		let (kind, message, status, code) = match e {
			Error::Unsupported(m) => ("unsupported", m.clone(), None, None),
			Error::Config(m) => ("config", m.clone(), None, None),
			Error::Api { status, code, message } => ("api", message.clone(), Some(*status), code.clone()),
			Error::Plugin { source, .. } => ("plugin", source.to_string(), None, None),
		};
		Self {
			kind: kind.into(),
			message,
			status,
			code,
		}
	}
}

impl WireError {
	/// `plugin` は、エラーを返してきたプラグインの名前。
	pub fn into_error(self, plugin: &'static str) -> Error {
		match self.kind.as_str() {
			"unsupported" => Error::Unsupported(self.message),
			"config" => Error::Config(self.message),
			"api" => Error::Api {
				status: self.status.unwrap_or(0),
				code: self.code,
				message: self.message,
			},
			_ => Error::plugin(plugin, self.message),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn errors_round_trip_through_the_wire() {
		let api = Error::Api {
			status: 429,
			code: Some("rate".into()),
			message: "slow down".into(),
		};
		let back = WireError::from(&api).into_error("p");
		assert!(
			matches!(back, Error::Api { status: 429, code: Some(c), message } if c == "rate" && message == "slow down")
		);

		let cfg = WireError::from(&Error::Config("x".into())).into_error("p");
		assert!(matches!(cfg, Error::Config(m) if m == "x"));

		let plugin = WireError::from(&Error::plugin("inner", "boom")).into_error("outer");
		assert_eq!(plugin.to_string(), "outer: boom");
	}

	#[test]
	fn empty_results_are_not_null() {
		let line = serde_json::to_string(&Frame::ok(1, Value::Null)).unwrap();
		let back: Frame = serde_json::from_str(&line).unwrap();
		assert_eq!(back.result, Some(serde_json::json!({})));
	}

	#[test]
	fn req_is_read_from_params() {
		let f = Frame::notify("stream.delta", serde_json::json!({"req": 7, "text": "a"}));
		assert_eq!(f.req(), Some(7));
	}
}
