//! Bounded, newline-framed local IPC. Each command owns its connection so polling
//! cannot consume another command's reply.

use dawg_core::{MAX_FRAME_BYTES, PROTOCOL_VERSION, Request, Response, socket_path};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn request(request: Request) -> Result<Response, String> {
    request_at(&socket_path(), request).await
}

/// Called at initial startup and explicit reconnect only. Regular requests never
/// spawn processes, preventing a failing daemon from causing a spawn loop.
pub async fn connect() -> Result<Response, String> {
    let path = socket_path();
    if let Ok(stream) = connect_stream(&path).await {
        return exchange_with_timeout(stream, Request::Snapshot).await;
    }

    let binary = daemon_binary();
    let mut child = tokio::process::Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "Cannot start the Dawg daemon ({}): {error}. Build both workspace binaries or start dawg-daemon manually.",
                binary.display()
            )
        })?;
    // Reap early failures while allowing the daemon to outlive this UI.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(stream) = connect_stream(&path).await {
            return exchange_with_timeout(stream, Request::Snapshot).await;
        }
    }
    Err(format!(
        "The daemon did not become available at {}. Run dawg-daemon in a terminal to inspect its startup error.",
        path.display()
    ))
}

fn daemon_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("dawg-daemon")))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("dawg-daemon"))
}

async fn connect_stream(path: &Path) -> Result<UnixStream, String> {
    tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| "Connection to the daemon timed out.".to_owned())?
        .map_err(|error| {
            format!(
                "Cannot connect to the daemon at {}: {error}",
                path.display()
            )
        })
}

async fn request_at(path: &Path, request: Request) -> Result<Response, String> {
    let stream = connect_stream(path).await?;
    exchange_with_timeout(stream, request).await
}

async fn exchange_with_timeout(stream: UnixStream, request: Request) -> Result<Response, String> {
    tokio::time::timeout(REQUEST_TIMEOUT, exchange(stream, request))
        .await
        .map_err(|_| "The daemon did not answer within 60 seconds. Check its console before retrying the operation.".to_owned())?
}

async fn exchange(mut stream: UnixStream, request: Request) -> Result<Response, String> {
    let mut frame = serde_json::to_vec(&request)
        .map_err(|error| format!("Cannot encode daemon request: {error}"))?;
    if frame.len() >= MAX_FRAME_BYTES {
        return Err("This request is too large for the local daemon protocol.".to_owned());
    }
    frame.push(b'\n');
    stream
        .write_all(&frame)
        .await
        .map_err(|error| format!("Cannot send the daemon request: {error}"))?;

    let mut frame = Vec::new();
    BufReader::new(stream.take((MAX_FRAME_BYTES + 1) as u64))
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|error| format!("Cannot read the daemon response: {error}"))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err("The daemon response exceeded the maximum frame size.".to_owned());
    }
    if frame.last() != Some(&b'\n') {
        return Err("The daemon closed the connection before completing its response.".to_owned());
    }
    let response: Response = serde_json::from_slice(&frame)
        .map_err(|error| format!("The daemon returned an invalid response: {error}"))?;
    if let Response::Snapshot(snapshot) = &response
        && snapshot.protocol_version != PROTOCOL_VERSION
    {
        return Err(format!(
            "Daemon protocol {} is incompatible with UI protocol {}. Rebuild and restart both binaries.",
            snapshot.protocol_version, PROTOCOL_VERSION
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::UnixListener;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dawg-ui-transport-{}-{}.sock",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    async fn serve_frame(listener: UnixListener, bytes: Vec<u8>) {
        let (stream, _) = listener.accept().await.expect("accept fixture connection");
        let (read, mut write) = stream.into_split();
        let mut request = Vec::new();
        BufReader::new(read)
            .read_until(b'\n', &mut request)
            .await
            .expect("read fixture request");
        assert!(matches!(
            serde_json::from_slice::<Request>(&request).expect("valid request"),
            Request::Snapshot
        ));
        // Force the client to assemble a frame from multiple reads.
        for chunk in bytes.chunks(997) {
            if write.write_all(chunk).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn assembles_large_fragmented_response() {
        let path = fixture_path();
        let listener = UnixListener::bind(&path).expect("bind fixture");
        let expected = "server-property=value\n".repeat(1200);
        let mut bytes = serde_json::to_vec(&Response::Properties {
            id: "test".to_owned(),
            content: expected.clone(),
        })
        .expect("serialize fixture");
        bytes.push(b'\n');
        let server = tokio::spawn(serve_frame(listener, bytes));
        let response = request_at(&path, Request::Snapshot)
            .await
            .expect("large response succeeds");
        assert!(matches!(response, Response::Properties { content, .. } if content == expected));
        server.await.expect("fixture completed");
        std::fs::remove_file(path).expect("remove fixture socket");
    }

    #[tokio::test]
    async fn rejects_incomplete_response() {
        let path = fixture_path();
        let listener = UnixListener::bind(&path).expect("bind fixture");
        let server = tokio::spawn(serve_frame(listener, b"\"Ok\"".to_vec()));
        let error = request_at(&path, Request::Snapshot)
            .await
            .expect_err("unterminated response rejected");
        assert!(error.contains("before completing"));
        server.await.expect("fixture completed");
        std::fs::remove_file(path).expect("remove fixture socket");
    }

    #[tokio::test]
    async fn rejects_oversized_response() {
        let path = fixture_path();
        let listener = UnixListener::bind(&path).expect("bind fixture");
        let server = tokio::spawn(serve_frame(listener, vec![b'x'; MAX_FRAME_BYTES + 2]));
        let error = request_at(&path, Request::Snapshot)
            .await
            .expect_err("oversized response rejected");
        assert!(error.contains("maximum frame size"));
        server.await.expect("fixture completed");
        std::fs::remove_file(path).expect("remove fixture socket");
    }
}
