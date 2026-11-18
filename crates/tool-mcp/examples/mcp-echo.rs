//! テスト用の小さな MCP サーバー (stdio)。
//!
//! - `echo { text }`: `text` をそのまま返す
//! - `fail`: `isError: true` で返す
//! - `where`: 作業ディレクトリと環境変数 `MCP_ECHO_TAG` を返す
//!
//! `tools/list` は2ページに分けて返す (`nextCursor` を追えるかを試すため)。

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() {
	let mut lines = BufReader::new(tokio::io::stdin()).lines();
	let mut stdout = tokio::io::stdout();
	eprintln!("mcp-echo: ready");
	while let Ok(Some(line)) = lines.next_line().await {
		let Ok(msg) = serde_json::from_str::<Value>(&line) else {
			continue;
		};
		let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(Value::as_str)) else {
			continue;
		};
		let params = msg.get("params").cloned().unwrap_or_default();
		let result = match method {
			"initialize" => json!({
				"protocolVersion": params["protocolVersion"],
				"capabilities": { "tools": {} },
				"serverInfo": { "name": "mcp-echo", "version": "0" },
			}),
			"tools/list" if params.get("cursor").is_none() => json!({
				"tools": [{
					"name": "echo",
					"description": "Echo the text back",
					"inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
				}],
				"nextCursor": "2",
			}),
			"tools/list" => json!({ "tools": [{ "name": "fail" }, { "name": "where", "title": "Where am I" }] }),
			"tools/call" => {
				let text = match params["name"].as_str() {
					Some("echo") => params["arguments"]["text"].as_str().unwrap_or_default().to_owned(),
					Some("fail") => {
						reply(
							&mut stdout,
							json!({ "jsonrpc": "2.0", "id": id, "result": {
								"content": [{ "type": "text", "text": "it failed" }], "isError": true,
							}}),
						)
						.await;
						continue;
					}
					Some("where") => format!(
						"{} {}",
						std::env::current_dir().unwrap().display(),
						std::env::var("MCP_ECHO_TAG").unwrap_or_default()
					),
					other => format!("unknown tool {other:?}"),
				};
				json!({ "content": [{ "type": "text", "text": text }] })
			}
			"ping" => json!({}),
			_ => {
				let error = json!({ "code": -32601, "message": format!("method not found: {method}") });
				reply(&mut stdout, json!({ "jsonrpc": "2.0", "id": id, "error": error })).await;
				continue;
			}
		};
		reply(&mut stdout, json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
	}
}

async fn reply(out: &mut tokio::io::Stdout, msg: Value) {
	let _ = out.write_all(format!("{msg}\n").as_bytes()).await;
	let _ = out.flush().await;
}
