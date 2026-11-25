//! Wire contracts for heterogeneous DSH plugin federation (`Cordis-over-Wire`)
//! and harness-agnostic projection (MCP, `inkstone.tools.v1`, ACP / `dsh-sdk-protocol`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const WIRE_SCHEMA: &str = "electron-cordis.wire.v1";
pub const INKSTONE_NATIVE_CONTRACT_PROTOCOL: &str = "inkstone-harness-rpc";
pub const INKSTONE_NATIVE_CONTRACT_SHA256: &str =
    "1bb31ba7b9cd5615b00da1e89632f34e2b425a7a8d65144a013bd6c1e4c627c8";
pub const INKSTONE_NATIVE_MAX_FRAME_BYTES: usize = 8_388_608;

/// ACP (`bridge-acp` / `/run/krun/dsh-rpc.sock`) session creation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpSessionCreate {
    #[serde(default = "default_acp_harness")]
    pub harness_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

fn default_acp_harness() -> String {
    "acp-client".to_string()
}

/// ACP prompt/tool execution request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpPromptRequest {
    pub session_id: String,
    pub prompt: String,
    #[serde(default)]
    pub tool_call: Option<AcpToolCallSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolCallSpec {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Tool definition registered by any DSH plugin (Rust, Node/TS, Wasm, or Electron cap)
/// into Plane B and projected to every harness running inside KRun.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedToolSpec {
    pub name: String,
    pub version: String,
    pub origin_plugin: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub requires_approval: bool,
}

/// Standard MCP `tools/list` descriptor projected by `bridge-mcp`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Standard MCP `tools/call` content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

/// Standard MCP `tools/call` result payload projected by `bridge-mcp`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpCallToolResult {
    pub content: Vec<McpContentBlock>,
    #[serde(rename = "isError")]
    pub is_error: bool,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

/// Inkstone Host Native Contract (`inkstone.tools.v1`) tool invocation envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeContractToolCall {
    #[serde(default = "default_native_session")]
    pub session_id: String,
    #[serde(default = "default_native_harness")]
    pub harness_id: String,
    pub tool: String,
    #[serde(default)]
    pub input: Value,
}

fn default_native_session() -> String {
    "inkstone-native-default".to_string()
}

fn default_native_harness() -> String {
    "inkstone-harness".to_string()
}

/// Control messages exchanged between a Rust Shadow Fiber and a remote plugin host
/// (`@dsh/cordis-bridge` in Electron UtilityProcess or KRun Guest Node).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ShadowFiberMessage {
    /// Host -> Remote: all `inject` services are ready; execute plugin `apply(ctx)`.
    Apply {
        fiber_id: String,
        generation: u64,
        plugin_id: String,
        config: Value,
    },
    /// Host -> Remote: hot-update config without breaking Fiber identity (`FiberHandle::update`).
    Update {
        fiber_id: String,
        generation: u64,
        config: Value,
    },
    /// Host -> Remote: drain all `ctx.effect` cleanups in reverse commit order (`FiberHandle::dispose`).
    Dispose {
        fiber_id: String,
        generation: u64,
    },
    /// Remote -> Host: plugin registered a tool via `ctx.tools.register(defineTool(...))`.
    RegisterTool {
        fiber_id: String,
        generation: u64,
        tool: ProjectedToolSpec,
    },
    /// Host -> Remote: execute a registered DSH tool inside the remote Shadow Fiber.
    ToolInvoke {
        request_id: String,
        fiber_id: String,
        tool_name: String,
        arguments: Value,
    },
    /// Remote -> Host: plugin invoked a bridged service (e.g. `ctx.shell`, `ctx.clipboard`).
    ServiceInvoke {
        request_id: String,
        fiber_id: String,
        service: String,
        method: String,
        params: Value,
    },
    /// Bidirectional response for `ServiceInvoke` or `ToolInvoke`.
    RpcReply {
        request_id: String,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    },
}

/// Protocol projection endpoints exposed into KRun Guest VMs over stable paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessProjection {
    /// `/run/krun/dsh-mcp.sock` — standard MCP server for any MCP-capable harness.
    Mcp,
    /// `/run/krun/host-tools.sock` (vsock 4051) — Inkstone Host Native Contract / `inkstone.tools.v1` bridge.
    InkstoneToolsV1,
    /// `/run/krun/dsh-rpc.sock` — ACP & `dsh-sdk-protocol` JSON-RPC 2.0 stream.
    AcpJsonRpc,
    /// `/run/krun/llm-gateway.sock` — unified LLM proxy chokepoint for DSH prompt/guardrail plugins.
    LlmGateway,
}

impl HarnessProjection {
    pub fn default_guest_socket(self) -> &'static str {
        match self {
            Self::Mcp => "/run/krun/dsh-mcp.sock",
            Self::InkstoneToolsV1 => "/run/krun/host-tools.sock",
            Self::AcpJsonRpc => "/run/krun/dsh-rpc.sock",
            Self::LlmGateway => "/run/krun/llm-gateway.sock",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn shadow_fiber_and_mcp_wire_roundtrip() {
        let msg = ShadowFiberMessage::ToolInvoke {
            request_id: "req-1".into(),
            fiber_id: "sf-1".into(),
            tool_name: "demo.greet".into(),
            arguments: json!({ "who": "inkstone" }),
        };
        let encoded = serde_json::to_string(&msg).unwrap();
        let decoded: ShadowFiberMessage = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            ShadowFiberMessage::ToolInvoke { ref tool_name, .. } if tool_name == "demo.greet"
        ));

        let mcp = McpToolDescriptor {
            name: "clipboard.available_formats".into(),
            description: "formats only".into(),
            input_schema: json!({ "type": "object" }),
        };
        let v = serde_json::to_value(&mcp).unwrap();
        assert_eq!(v["inputSchema"], json!({ "type": "object" }));
    }
}
