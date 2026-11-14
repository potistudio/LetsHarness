//! ループ型のエージェント。
//!
//! 「エージェントループを誰が回すか」で `Provider` と分かれる `AgentBackend` の一種で、
//! こちらがループを回し、モデルとツールは **kernel 経由で借りる** (他のプラグインの crate は知らない)。
//!
//! `cfg`:
//! - `provider` (必須): 使う Provider の kind。
//! - それ以外 (`workdir` や `-o` で渡した値など) は Provider / ツール供給元にそのまま渡る。

mod run;

use std::sync::Arc;

use letsharness_kernel::{
	AgentBackend, AgentBackendFactory, AgentHandler, AgentOutcome, AgentRequest, Error, ModelInfo, PluginServer, Result,
};
use serde_json::Value;

pub use run::run_agent;

pub fn server() -> PluginServer {
	PluginServer::new().agent(LoopFactory)
}

struct LoopFactory;

impl AgentBackendFactory for LoopFactory {
	fn kind(&self) -> &'static str {
		"loop"
	}

	fn create(&self, cfg: &Value) -> Result<Arc<dyn AgentBackend>> {
		let provider = cfg
			.get("provider")
			.and_then(Value::as_str)
			.ok_or_else(|| Error::Config("loop: cfg.provider (Provider の kind) が必要です".into()))?
			.to_owned();
		Ok(Arc::new(LoopAgent {
			provider,
			cfg: cfg.clone(),
		}))
	}
}

pub struct LoopAgent {
	provider: String,
	cfg: Value,
}

#[async_trait::async_trait]
impl AgentBackend for LoopAgent {
	fn name(&self) -> &'static str {
		"loop"
	}

	async fn list_models(&self) -> Result<Vec<ModelInfo>> {
		// モデル一覧は kernel 経由でしか借りられず、その窓口は実行中の `run` にしか無い。
		Err(Error::Unsupported(
			"list_models (`--provider` でモデルを引いてください)".into(),
		))
	}

	async fn run(&self, req: AgentRequest, handler: &mut dyn AgentHandler) -> Result<AgentOutcome> {
		if req.session_id.is_some() {
			return Err(Error::Unsupported(
				"session_id (ループは AgentOutcome::messages を渡して継続します)".into(),
			));
		}
		if req.model.is_empty() {
			return Err(Error::Config("loop: model が必要です".into()));
		}
		let (provider, tools) = {
			let host = handler
				.host()
				.ok_or_else(|| Error::Unsupported("loop: kernel 経由でしかモデルとツールを借りられません".into()))?;
			(
				host.provider(&self.provider, &self.cfg).await?,
				host.tools(&self.cfg).await?,
			)
		};
		run_agent(&*provider, &tools, handler, req).await
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use letsharness_kernel::{ChatMessage, Completion, HostApi, Provider, ToolRegistry, ToolSpec};
	use serde_json::json;

	use super::*;

	/// `Completion` を台本どおりに返すだけの Provider。
	struct Scripted(Mutex<Vec<Completion>>);

	#[async_trait::async_trait]
	impl Provider for Scripted {
		fn name(&self) -> &'static str {
			"scripted"
		}
		async fn list_models(&self) -> Result<Vec<ModelInfo>> {
			Ok(vec![])
		}
		async fn chat(&self, _: &str, _: Vec<ChatMessage>, _: Option<u32>) -> Result<String> {
			unimplemented!()
		}
		async fn chat_stream(
			&self,
			_: &str,
			_: Vec<ChatMessage>,
			_: &mut (dyn for<'a> FnMut(&'a str) + Send),
			_: &mut (dyn for<'a> FnMut(&'a str) + Send),
		) -> Result<String> {
			unimplemented!()
		}
		async fn responses(&self, _: &str, _: &str) -> Result<String> {
			unimplemented!()
		}
		async fn messages(&self, _: &str, _: Vec<ChatMessage>, _: Option<&str>, _: u32) -> Result<String> {
			unimplemented!()
		}
		async fn chat_with_tools(
			&self,
			_: &str,
			_: Vec<ChatMessage>,
			_: &[ToolSpec],
			_: Option<u32>,
		) -> Result<Completion> {
			Ok(self.0.lock().unwrap().remove(0))
		}
	}

	/// kernel の代わりに、決まった Provider を貸す。
	struct FakeHost;

	#[async_trait::async_trait]
	impl HostApi for FakeHost {
		async fn provider(&self, kind: &str, _cfg: &Value) -> Result<Arc<dyn Provider>> {
			assert_eq!(kind, "scripted");
			Ok(Arc::new(Scripted(Mutex::new(vec![Completion {
				text: "ok".into(),
				..Default::default()
			}]))))
		}
		async fn tools(&self, _cfg: &Value) -> Result<ToolRegistry> {
			Ok(ToolRegistry::new())
		}
	}

	struct Handler(Option<FakeHost>);

	#[async_trait::async_trait]
	impl AgentHandler for Handler {
		fn host(&self) -> Option<&dyn HostApi> {
			self.0.as_ref().map(|h| h as &dyn HostApi)
		}
	}

	fn agent() -> Arc<dyn AgentBackend> {
		LoopFactory.create(&json!({ "provider": "scripted" })).unwrap()
	}

	fn req() -> AgentRequest {
		AgentRequest::new("m", vec![ChatMessage::user("hi")])
	}

	#[test]
	fn provider_is_required() {
		assert!(matches!(LoopFactory.create(&json!({})), Err(Error::Config(_))));
	}

	#[tokio::test]
	async fn borrows_model_and_tools_from_the_kernel() {
		let out = agent().run(req(), &mut Handler(Some(FakeHost))).await.unwrap();
		assert_eq!((out.text.as_str(), out.steps), ("ok", 1));
		assert_eq!(out.session_id, None);
	}

	#[tokio::test]
	async fn rejects_runs_without_a_kernel_link_session_ids_and_missing_models() {
		let a = agent();
		assert!(matches!(
			a.run(req(), &mut Handler(None)).await,
			Err(Error::Unsupported(_))
		));

		let mut r = req();
		r.session_id = Some("s".into());
		assert!(matches!(
			a.run(r, &mut Handler(Some(FakeHost))).await,
			Err(Error::Unsupported(_))
		));

		let mut r = req();
		r.model.clear();
		assert!(matches!(
			a.run(r, &mut Handler(Some(FakeHost))).await,
			Err(Error::Config(_))
		));
	}
}
