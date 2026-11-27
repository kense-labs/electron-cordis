//! Stage A local microkernel daemon (`electron-cordis-daemon`).
//!
//! Speaks newline-delimited JSON-RPC 2.0 over stdio and optional local transports.
//! Use a Unix-domain socket path or a Windows named pipe path.
//! - `--load-plan <path>`: frozen LoadPlan manifest produced by `script/gen-cordis-build.py`
//! - `--socket <path>`: general Narrow Dock JSON-RPC 2.0 endpoint
//! - `--mcp-socket <path>`: standard MCP projection (`bridge-mcp`, `/run/krun/dsh-mcp.sock`)
//! - `--native-tools-socket <path>`: Inkstone Host Native Contract (`inkstone.tools.v1`)
//! - `--acp-socket <path>`: ACP / `dsh-sdk-protocol` (`bridge-acp`, `/run/krun/dsh-rpc.sock`)
//! - `--llm-gateway-socket <path>`: LLM Proxy chokepoint (`llm-gateway`, `/run/krun/llm-gateway.sock`)
//! - `--shadow-socket <path>`: bidirectional `ShadowFiberMessage` bridge for `@dsh/cordis-bridge`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use electron_cordis_daemon::{DaemonState, RpcError, RpcRequest, RpcResponse};
use electron_cordis_kernel::LoadPlanManifest;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct CliArgs {
    load_plan: Option<PathBuf>,
    #[cfg(unix)]
    writer_seal_fd: Option<String>,
    socket: Option<PathBuf>,
    mcp_socket: Option<PathBuf>,
    native_tools_socket: Option<PathBuf>,
    acp_socket: Option<PathBuf>,
    llm_gateway_socket: Option<PathBuf>,
    shadow_socket: Option<PathBuf>,
}

fn parse_cli_args() -> CliArgs {
    let mut args = env::args().skip(1);
    let mut cli = CliArgs {
        load_plan: None,
        #[cfg(unix)]
        writer_seal_fd: None,
        socket: None,
        mcp_socket: None,
        native_tools_socket: None,
        acp_socket: None,
        llm_gateway_socket: None,
        shadow_socket: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--load-plan" => cli.load_plan = args.next().map(PathBuf::from),
            #[cfg(unix)]
            "--writer-seal-fd" => cli.writer_seal_fd = Some(args.next().unwrap_or_default()),
            "--socket" => cli.socket = args.next().map(PathBuf::from),
            "--mcp-socket" => cli.mcp_socket = args.next().map(PathBuf::from),
            "--native-tools-socket" => cli.native_tools_socket = args.next().map(PathBuf::from),
            "--acp-socket" => cli.acp_socket = args.next().map(PathBuf::from),
            "--llm-gateway-socket" => cli.llm_gateway_socket = args.next().map(PathBuf::from),
            "--shadow-socket" => cli.shadow_socket = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    cli
}

fn load_plan(path: Option<PathBuf>) -> Result<Option<LoadPlanManifest>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let plan = LoadPlanManifest::from_json_str(&raw).map_err(|e| e.to_string())?;
    Ok(Some(plan))
}

#[cfg(unix)]
fn retain_writer_seal(value: Option<&str>) -> std::io::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let fd = value.parse::<std::os::fd::RawFd>().ok().filter(|fd| *fd > 2).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid inherited writer seal descriptor",
        )
    })?;
    // SAFETY: fcntl validates the descriptor without taking or duplicating ownership.
    // Keep the inherited descriptor until exit and close it in spawned plugins.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = parse_cli_args();
    #[cfg(unix)]
    retain_writer_seal(cli.writer_seal_fd.as_deref())?;
    let plan = load_plan(cli.load_plan).map_err(|e| format!("fail-closed startup: {e}"))?;
    let state = Arc::new(DaemonState::start(plan).await?);

    #[cfg(any(unix, windows))]
    let mut listener_handles = Vec::new();
    #[cfg(unix)]
    let mut bound_sockets = Vec::new();

    #[cfg(any(unix, windows))]
    {
        for sock in [
            cli.socket,
            cli.mcp_socket,
            cli.native_tools_socket,
            cli.acp_socket,
            cli.llm_gateway_socket,
        ]
        .into_iter()
        .flatten()
        {
            #[cfg(unix)]
            {
                listener_handles.push(state.spawn_jsonrpc_unix_listener(sock.clone())?);
                bound_sockets.push(sock);
            }
            #[cfg(windows)]
            listener_handles.push(state.spawn_jsonrpc_named_pipe_listener(sock)?);
        }
        if let Some(shadow_sock) = cli.shadow_socket {
            #[cfg(unix)]
            {
                listener_handles.push(state.spawn_shadow_unix_listener(shadow_sock.clone())?);
                bound_sockets.push(shadow_sock);
            }
            #[cfg(windows)]
            listener_handles.push(state.spawn_shadow_named_pipe_listener(shadow_sock)?);
        }
    }

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, should_exit) = match serde_json::from_str::<RpcRequest>(trimmed) {
            Ok(req) => state.dispatch(req).await,
            Err(e) => (
                RpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("invalid json: {e}"),
                    }),
                },
                false,
            ),
        };
        let encoded = serde_json::to_string(&resp)?;
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
        if should_exit {
            break;
        }
    }

    #[cfg(any(unix, windows))]
    for h in listener_handles {
        h.abort();
    }
    #[cfg(unix)]
    for sock in bound_sockets {
        let _ = fs::remove_file(sock);
    }

    Ok(())
}
