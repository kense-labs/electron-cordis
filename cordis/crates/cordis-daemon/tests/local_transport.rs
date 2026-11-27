#![cfg(any(unix, windows))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use electron_cordis_daemon::DaemonState;
use electron_cordis_wire::ShadowFiberMessage;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};

fn endpoint(label: &str) -> PathBuf {
    let timestamp = SystemTime::UNIX_EPOCH.elapsed().expect("wall clock").as_nanos();
    let name = format!("cordis-test-{}-{timestamp}-{label}", std::process::id());
    #[cfg(unix)]
    return Path::new("/tmp").join(name);
    #[cfg(windows)]
    return PathBuf::from(format!(r"\\.\pipe\{name}"));
}

#[cfg(unix)]
async fn connect(path: &Path) -> tokio::net::UnixStream {
    tokio::net::UnixStream::connect(path).await.expect("connect to Unix socket")
}

#[cfg(windows)]
async fn connect(path: &Path) -> tokio::net::windows::named_pipe::NamedPipeClient {
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return client,
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("connect to named pipe: {error}"),
            }
        }
    })
    .await
    .expect("available named pipe instance")
}

#[tokio::test]
async fn rpc_handles_disconnected_peers_and_buffered_requests() {
    let state = Arc::new(DaemonState::start(None).await.expect("daemon"));
    let path = endpoint("rpc");
    #[cfg(unix)]
    let listener = state.spawn_jsonrpc_unix_listener(path.clone()).expect("bind socket");
    #[cfg(windows)]
    let listener = state.spawn_jsonrpc_named_pipe_listener(path.clone()).expect("bind pipe");
    drop(connect(&path).await);
    let mut first = BufReader::new(connect(&path).await).lines();
    let mut second = BufReader::new(connect(&path).await).lines();
    let request = |id| json!({"jsonrpc":"2.0","id":id,"method":"cordis.health","params":{}});
    first
        .get_mut()
        .get_mut()
        .write_all(format!("invalid\n{}\n{}\n", request(1), request(2)).as_bytes())
        .await
        .expect("write buffered requests");
    second
        .get_mut()
        .get_mut()
        .write_all(format!("{}\n", request(3)).as_bytes())
        .await
        .expect("write second connection");

    let error: Value =
        serde_json::from_str(&reply(&mut first).await).expect("parse error response");
    assert_eq!(error["error"]["code"], -32700);
    for id in 1..=3 {
        let reader = if id == 3 { &mut second } else { &mut first };
        let result: Value =
            serde_json::from_str(&reply(reader).await).expect("parse health response");
        assert_eq!(result["id"], id);
        assert_eq!(result["result"]["profile"], "unconfigured");
    }
    listener.abort();
    #[cfg(unix)]
    std::fs::remove_file(path).expect("remove socket");
}

#[cfg(windows)]
#[tokio::test]
async fn named_pipe_rejects_a_second_owner() {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    let state = Arc::new(DaemonState::start(None).await.expect("daemon"));
    let path = endpoint("duplicate-owner");
    let listener = state.spawn_jsonrpc_named_pipe_listener(path.clone()).expect("bind pipe");
    let error = state.spawn_jsonrpc_named_pipe_listener(path).expect_err("reject a second owner");
    assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    listener.abort();
}

#[tokio::test]
async fn shadow_returns_rpc_reply_on_the_same_connection() {
    let state = Arc::new(DaemonState::start(None).await.expect("daemon"));
    let path = endpoint("shadow");
    #[cfg(unix)]
    let listener = state.spawn_shadow_unix_listener(path.clone()).expect("bind socket");
    #[cfg(windows)]
    let listener = state.spawn_shadow_named_pipe_listener(path.clone()).expect("bind pipe");
    let mut connection = BufReader::new(connect(&path).await).lines();
    let request = ShadowFiberMessage::ServiceInvoke {
        request_id: "local-request".into(),
        fiber_id: "test-fiber".into(),
        service: "unknown-capability".into(),
        method: "unknown-method".into(),
        params: json!({}),
    };
    let request = serde_json::to_string(&request).expect("encode request");
    connection
        .get_mut()
        .get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write shadow request");
    let response = serde_json::from_str(&reply(&mut connection).await).expect("parse shadow reply");
    assert!(matches!(response, ShadowFiberMessage::RpcReply { request_id, ok: false, .. }
        if request_id == "local-request"));
    listener.abort();
    #[cfg(unix)]
    std::fs::remove_file(path).expect("remove socket");
}

async fn reply(stream: &mut tokio::io::Lines<BufReader<impl AsyncRead + Unpin>>) -> String {
    tokio::time::timeout(Duration::from_secs(5), stream.next_line())
        .await
        .expect("daemon responds")
        .expect("read response")
        .expect("response line")
}
