//! M4 heterogeneous federation integration test:
//! Proves that a Rust capability plugin (`@dsh/cap-clipboard`) and a remote JS DSH plugin
//! mounted via `ShadowFiber` register into a single Plane B `ToolCatalog`, are transparently
//! discovered and invoked by BOTH an external MCP client (`tools/list`, `tools/call`) AND an
//! Inkstone Host Native Contract client (`inkstone.tools.list`, `inkstone.tools.call`), share
//! one unified `Around` approval guardrail + trajectory, and obey reverse-order cleanup on dispose.

use std::sync::{Arc, Mutex};

use electron_cordis_daemon::{DaemonState, RemoteToolExecutor, RpcRequest};
use electron_cordis_wire::{ProjectedToolSpec, ShadowFiberMessage};
use serde_json::{Value, json};

async fn rpc(state: &DaemonState, method: &str, params: Value) -> Value {
    let (resp, _) = state
        .dispatch(RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: method.into(),
            params,
        })
        .await;
    assert!(
        resp.error.is_none(),
        "unexpected rpc error on {method}: {:?}",
        resp.error
    );
    resp.result.expect("rpc result")
}

#[tokio::test]
async fn mcp_and_native_contract_share_rust_and_js_shadow_tools_with_unified_around_and_teardown() {
    let state = DaemonState::start(None).await.expect("start daemon");

    // 1. Mount a simulated DSH JS plugin over ShadowFiberMessage (Apply + RegisterTool).
    let cleanup_log = Arc::new(Mutex::new(Vec::<String>::new()));
    let exec: RemoteToolExecutor = Arc::new(|_fid, tool_name, args| {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("empty");
            Ok(json!({
                "tool": tool_name,
                "summary": format!("chars={}", text.len()),
            }))
        })
    });
    state.attach_shadow_executor("sf-js-1", exec);

    state
        .handle_shadow_message(ShadowFiberMessage::Apply {
            fiber_id: "sf-js-1".into(),
            generation: 1,
            plugin_id: "@deepseek-ai/dsh-demo-summarizer".into(),
            config: json!({}),
        })
        .await
        .expect("shadow apply");

    state
        .handle_shadow_message(ShadowFiberMessage::RegisterTool {
            fiber_id: "sf-js-1".into(),
            generation: 1,
            tool: ProjectedToolSpec {
                name: "dsh.summarize".into(),
                version: "1".into(),
                origin_plugin: "@deepseek-ai/dsh-demo-summarizer".into(),
                description: "Summarize text via JS DSH plugin".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
                requires_approval: true,
            },
        })
        .await
        .expect("shadow register tool");

    // 2. External MCP client discovers BOTH Rust cap-clipboard and JS dsh.summarize.
    let mcp_init = rpc(&state, "initialize", json!({})).await;
    assert_eq!(mcp_init["serverInfo"]["name"], json!("electron-cordis-bridge-mcp"));

    let mcp_list = rpc(&state, "tools/list", json!({})).await;
    let mcp_names: Vec<&str> = mcp_list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(mcp_names.contains(&"clipboard.available_formats"));
    assert!(mcp_names.contains(&"clipboard.write_text"));
    assert!(mcp_names.contains(&"dsh.summarize"));

    // 3. Host Native Contract (`inkstone.tools.v1`) discovers the exact same tool set.
    let native_list = rpc(&state, "inkstone.tools.list", json!({})).await;
    let native_names: Vec<&str> = native_list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(mcp_names, native_names);

    // 4. Both MCP and Host Native Contract invoke Rust and JS tools transparently.
    let mcp_write = rpc(
        &state,
        "tools/call",
        json!({
            "name": "clipboard.write_text",
            "arguments": { "text": "hello-federation" }
        }),
    )
    .await;
    assert_eq!(mcp_write["isError"], json!(false));

    let native_formats = rpc(
        &state,
        "inkstone.tools.call",
        json!({
            "session_id": "s-native",
            "harness_id": "inkstone-harness",
            "tool": "clipboard.available_formats",
            "input": {}
        }),
    )
    .await;
    assert_eq!(native_formats["ok"], json!(true));
    assert_eq!(native_formats["output"]["formats"], json!(["text/plain"]));

    let mcp_js = rpc(
        &state,
        "tools/call",
        json!({
            "name": "dsh.summarize",
            "arguments": { "text": "hello-federation" }
        }),
    )
    .await;
    assert_eq!(mcp_js["isError"], json!(false));
    assert_eq!(mcp_js["structuredContent"]["summary"], json!("chars=16"));

    // 5. Unified Around approval middleware intercepts BOTH MCP and Host Native Contract!
    rpc(
        &state,
        "cordis.policy.set",
        json!({
            "enforce_requires_approval": true,
            "approved_tools": []
        }),
    )
    .await;

    let mcp_blocked = rpc(
        &state,
        "tools/call",
        json!({
            "name": "dsh.summarize",
            "arguments": { "text": "blocked" }
        }),
    )
    .await;
    assert_eq!(mcp_blocked["isError"], json!(true));
    assert!(mcp_blocked["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("APPROVAL_REQUIRED"));

    let native_blocked = rpc(
        &state,
        "inkstone.tools.call",
        json!({
            "session_id": "s-native",
            "harness_id": "inkstone-harness",
            "tool": "clipboard.read_text",
            "input": {}
        }),
    )
    .await;
    assert_eq!(native_blocked["ok"], json!(false));
    assert!(native_blocked["error"]
        .as_str()
        .unwrap()
        .contains("APPROVAL_REQUIRED"));

    // Non-approval format inspection still passes cleanly without leaking clipboard body.
    let fmt_ok = rpc(
        &state,
        "tools/call",
        json!({ "name": "clipboard.available_formats", "arguments": {} }),
    )
    .await;
    assert_eq!(fmt_ok["isError"], json!(false));
    assert!(!serde_json::to_string(&fmt_ok).unwrap().contains("hello-federation"));

    // Trajectory records captured BOTH harnesses and both executed/denied outcomes.
    let traj = state.trajectory_snapshot();
    assert!(traj.iter().any(|r| r.harness_id == "mcp-client" && r.kind == "tool.denied"));
    assert!(traj.iter().any(|r| r.harness_id == "inkstone-harness" && r.kind == "tool.denied"));

    // 6. Shadow Fiber Dispose unregisters JS tools immediately; shutdown drains Rust fibers in reverse order.
    let disposed = state
        .handle_shadow_message(ShadowFiberMessage::Dispose {
            fiber_id: "sf-js-1".into(),
            generation: 1,
        })
        .await
        .expect("shadow dispose");
    assert_eq!(disposed["removed_tools"], json!(1));
    cleanup_log.lock().unwrap().push("shadow_disposed".into());

    let after_dispose = rpc(&state, "tools/list", json!({})).await;
    let remaining: Vec<&str> = after_dispose["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(!remaining.contains(&"dsh.summarize"));
    assert!(remaining.contains(&"clipboard.available_formats"));

    let (shutdown_resp, should_exit) = state
        .dispatch(RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(99)),
            method: "cordis.shutdown".into(),
            params: json!({}),
        })
        .await;
    assert!(should_exit);
    assert_eq!(shutdown_resp.result.unwrap()["drained"], json!(true));
    assert!(state.tool_catalog().list().is_empty());
}

#[tokio::test]
async fn acp_projection_llm_gateway_redaction_and_startup_intent_fifo_all_work_together() {
    let state = DaemonState::start(None).await.expect("start daemon");

    // 1. Inkstone Native Contract metadata matches core/contracts/native/index.json
    let info = rpc(&state, "inkstone.contract.info", json!({})).await;
    assert_eq!(info["protocol"], json!("inkstone-harness-rpc"));
    assert_eq!(
        info["contractSha256"],
        json!("1bb31ba7b9cd5615b00da1e89632f34e2b425a7a8d65144a013bd6c1e4c627c8")
    );

    // 2. Startup Intent FIFO buffered before UiReady and drained deterministically on UiReady!
    rpc(
        &state,
        "cordis.compat.call",
        json!({
            "capability": "osIntegration",
            "method": "enqueueStartupIntent",
            "args": { "kind": "deep-link", "payload": "inkstone-cordis://task/1" }
        }),
    )
    .await;
    rpc(
        &state,
        "cordis.compat.call",
        json!({
            "capability": "osIntegration",
            "method": "enqueueStartupIntent",
            "args": { "kind": "open-file", "payload": "/tmp/a.pdf" }
        }),
    )
    .await;
    let ui_ready = rpc(&state, "cordis.ui_ready", json!({})).await;
    let intents = ui_ready["startup_intents"].as_array().unwrap();
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[0]["kind"], json!("deep-link"));
    assert_eq!(intents[1]["kind"], json!("open-file"));

    // 3. ACP projection (`bridge-acp`) creates session and executes Plane B Rust tool under unified Around policy!
    let sess = rpc(
        &state,
        "session/new",
        json!({ "harness_id": "acp-python-agent", "cwd": "/workspace" }),
    )
    .await;
    let sid = sess["sessionId"].as_str().unwrap().to_string();

    let acp_prompt = rpc(
        &state,
        "session/prompt",
        json!({
            "session_id": sid,
            "prompt": "inspect clipboard formats",
            "tool_call": { "name": "clipboard.available_formats", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(acp_prompt["stopReason"], json!("end_turn"));
    assert_eq!(acp_prompt["toolResult"]["ok"], json!(true));

    // 4. LLM Gateway second chokepoint: Mapper redacts secrets + Around enforces model allowlist!
    let llm_ok = rpc(
        &state,
        "llm.chat.completions",
        json!({
            "session_id": sid,
            "harness_id": "acp-python-agent",
            "model": "intern-s2.1-flash",
            "messages": [
                { "role": "user", "content": "my token is sk-secret12345 please help" }
            ]
        }),
    )
    .await;
    assert_eq!(llm_ok["ok"], json!(true));
    assert!(llm_ok["content"].as_str().unwrap().contains("[REDACTED]"));
    assert!(!llm_ok["content"].as_str().unwrap().contains("sk-secret12345"));
    assert_eq!(llm_ok["redacted_fields"], json!(["messages[0].content"]));

    // Block disallowed model via Around guardrail
    rpc(
        &state,
        "cordis.llm_policy.set",
        json!({ "allowed_models": ["intern-s2.1-flash"] }),
    )
    .await;
    let llm_denied = rpc(
        &state,
        "llm.chat.completions",
        json!({
            "session_id": sid,
            "harness_id": "acp-python-agent",
            "model": "unapproved-external-model",
            "messages": [{ "role": "user", "content": "hi" }]
        }),
    )
    .await;
    assert_eq!(llm_denied["ok"], json!(false));
    assert!(llm_denied["error"]
        .as_str()
        .unwrap()
        .contains("LLM_MODEL_NOT_ALLOWED"));

    // Unified trajectory captured ACP tool execution + LLM completion + LLM denial!
    let traj = state.trajectory_snapshot();
    assert!(traj
        .iter()
        .any(|r| r.harness_id == "acp-python-agent" && r.kind == "tool.executed"));
    assert!(traj
        .iter()
        .any(|r| r.origin_plugin == "@dsh/llm-gateway" && r.kind == "llm.completed"));
    assert!(traj
        .iter()
        .any(|r| r.origin_plugin == "@dsh/llm-gateway" && r.kind == "llm.denied"));

    let _ = state
        .dispatch(RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(100)),
            method: "cordis.shutdown".into(),
            params: json!({}),
        })
        .await;
}
