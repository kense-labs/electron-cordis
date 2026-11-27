use electron_cordis_daemon::{DaemonState, RpcRequest, RpcResponse};
use electron_cordis_kernel::LoadPlanManifest;
use serde_json::{json, Value};

fn plan(entries: Value) -> LoadPlanManifest {
    LoadPlanManifest::from_json_str(
        &json!({
            "schema": "cordis.load-plan.v1",
            "profile": "test",
            "entries": entries
        })
        .to_string(),
    )
    .expect("valid test manifest")
}

async fn rpc(state: &DaemonState, method: &str, params: Value) -> RpcResponse {
    state
        .dispatch(RpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(json!(1)),
            method: method.into(),
            params,
        })
        .await
        .0
}

#[tokio::test]
async fn on_demand_builtin_is_neither_loaded_nor_exposed() {
    let state = DaemonState::start(Some(plan(json!([{
        "id": "@dsh/cap-clipboard",
        "runtime": "rust",
        "isolation": "trusted-host",
        "realm": "host",
        "delivery": "on-demand"
    }]))))
    .await
    .expect("on-demand plugin can be omitted");
    let health = rpc(&state, "cordis.health", Value::Null)
        .await
        .result
        .unwrap();
    assert_eq!(health["loaded_builtin_plugins"], json!([]));
    assert_eq!(health["on_demand_plugins"], 1);

    let result = rpc(
        &state,
        "cordis.compat.call",
        json!({"capability":"clipboard", "method":"readText", "args": [], "realm": "host"}),
    )
    .await;
    assert_eq!(result.error.unwrap().code, -32004);
}

#[cfg(feature = "cap-os-integration")]
#[tokio::test]
async fn selected_builtin_receives_profile_config_and_is_disposed_on_shutdown() {
    use electron_cordis_cap_desktop_ui::OsIntegrationService;

    let state = DaemonState::start(Some(plan(json!([{
        "id": "@dsh/cap-os-integration",
        "runtime": "rust",
        "isolation": "trusted-host",
        "realm": "host",
        "config": {"locale": "en-US"}
    }]))))
    .await
    .expect("bundled plugin loads");
    let health = rpc(&state, "cordis.health", Value::Null)
        .await
        .result
        .unwrap();
    assert_eq!(
        health["loaded_builtin_plugins"],
        json!(["@dsh/cap-os-integration"])
    );
    let result = rpc(
        &state,
        "cordis.compat.call",
        json!({"capability":"osIntegration","method":"getLocale","args":[],"realm":"host"}),
    )
    .await;
    assert_eq!(result.result.unwrap()["locale"], "en-US");
    rpc(&state, "cordis.shutdown", Value::Null).await;
    assert!(state
        .topology()
        .host
        .try_service::<OsIntegrationService>()
        .is_err());
}

#[cfg(not(feature = "cap-clipboard"))]
#[tokio::test]
async fn incompatible_binary_rejects_bundled_plugin() {
    let err = DaemonState::start(Some(plan(json!([{
        "id": "@dsh/cap-clipboard",
        "runtime": "rust",
        "isolation": "trusted-host",
        "realm": "host"
    }]))))
    .await
    .err()
    .expect("daemon must reject the manifest");
    assert!(
        err.contains("Cargo feature `cap-clipboard` is unavailable"),
        "{err}"
    );
}

#[cfg(feature = "cap-os-integration")]
#[tokio::test]
async fn missing_declared_service_keeps_plugin_pending() {
    let state = DaemonState::start(Some(plan(json!([{
        "id": "@dsh/cap-os-integration",
        "runtime": "rust",
        "isolation": "trusted-host",
        "realm": "host",
        "inject": ["test.missing"]
    }]))))
    .await
    .expect("injected dependency can converge after startup");
    let health = rpc(&state, "cordis.health", Value::Null).await.result.unwrap();
    assert_eq!(health["loaded_builtin_plugins"], json!([]));
    rpc(&state, "cordis.shutdown", Value::Null).await;
}
