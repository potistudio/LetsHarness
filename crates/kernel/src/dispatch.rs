//! JSON のメソッド名・引数から `dyn Provider` を呼ぶ。
//! プラグイン側 (Kernel からの呼び出し) と kernel 側 (プラグインに借りられた呼び出し) の両方で使う。

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{ChatMessage, Error, Provider, Result, ToolSpec};

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
	Error::Config(format!("bad params `{what}`: {e}"))
}

/// `params[key]` を型に直す。無ければ `null` として扱う (`Option` なら `None`)。
pub(crate) fn field<T: DeserializeOwned>(params: &Value, key: &str) -> Result<T> {
	serde_json::from_value(params.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| bad(key, e))
}

pub(crate) fn to_value<T: serde::Serialize>(v: T) -> Result<Value> {
	serde_json::to_value(v).map_err(|e| Error::plugin("kernel", e))
}

/// `Provider` のメソッドを名前で呼ぶ。ストリームの差分は `on_delta` / `on_thinking` に流れる。
pub(crate) async fn call_provider(
	p: &dyn Provider,
	method: &str,
	params: &Value,
	on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
	on_thinking: &mut (dyn for<'a> FnMut(&'a str) + Send),
) -> Result<Value> {
	let model = || field::<String>(params, "model");
	let messages = || field::<Vec<ChatMessage>>(params, "messages");
	let max_tokens = || field::<Option<u32>>(params, "max_tokens");
	let system = || field::<Option<String>>(params, "system");

	match method {
		"list_models" => to_value(p.list_models().await?),
		"chat" => to_value(p.chat(&model()?, messages()?, max_tokens()?).await?),
		"chat_stream" => to_value(p.chat_stream(&model()?, messages()?, on_delta, on_thinking).await?),
		"responses" => to_value(p.responses(&model()?, &field::<String>(params, "input")?).await?),
		"messages" => {
			let max = field::<u32>(params, "max_tokens")?;
			to_value(p.messages(&model()?, messages()?, system()?.as_deref(), max).await?)
		}
		"quota" => to_value(p.quota().await?),
		"chat_with_usage" => to_value(p.chat_with_usage(&model()?, messages()?, max_tokens()?).await?),
		"chat_stream_with_usage" => to_value(
			p.chat_stream_with_usage(&model()?, messages()?, on_delta, on_thinking)
				.await?,
		),
		"chat_with_tools" => {
			let tools = field::<Vec<ToolSpec>>(params, "tools")?;
			to_value(p.chat_with_tools(&model()?, messages()?, &tools, max_tokens()?).await?)
		}
		"responses_with_usage" => to_value(
			p.responses_with_usage(&model()?, &field::<String>(params, "input")?)
				.await?,
		),
		"messages_with_usage" => {
			let max = field::<u32>(params, "max_tokens")?;
			to_value(
				p.messages_with_usage(&model()?, messages()?, system()?.as_deref(), max)
					.await?,
			)
		}
		other => Err(Error::Unsupported(format!("provider.{other}"))),
	}
}
