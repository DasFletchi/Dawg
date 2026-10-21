pub mod metadata;
mod network;
pub mod server;
mod tunnel;
mod web;

use dawg_core::{MAX_FRAME_BYTES, Request, Response};
use server::manager::ServerManager;
use std::{
    fs::OpenOptions,
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::Path,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Dawg: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let socket = dawg_core::socket_path();
    let parent = socket
        .parent()
        .ok_or("The daemon socket must have a parent directory")?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create runtime directory {}: {e}", parent.display()))?;
    // Never change permissions on a user-supplied existing directory.
    let lock_path = socket.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("Cannot open daemon lock: {e}"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another Dawg daemon is already using this socket.".into());
    }
    if socket.exists() {
        if !std::fs::symlink_metadata(&socket)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_socket()
        {
            return Err(
                "The socket path already exists and is not a Unix socket. It was not removed."
                    .into(),
            );
        }
        if UnixStream::connect(&socket).await.is_ok() {
            return Err("A daemon is already listening at this socket.".into());
        }
        std::fs::remove_file(&socket).map_err(|e| e.to_string())?;
    }
    let data_dir = dawg_core::data_dir();
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let data_dir = std::fs::canonicalize(data_dir).map_err(|error| error.to_string())?;
    let data_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(data_dir.join("daemon.lock"))
        .map_err(|error| error.to_string())?;
    if unsafe { libc::flock(data_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another Dawg daemon is already managing this data directory.".into());
    }
    let manager = Arc::new(ServerManager::load(data_dir.clone()).await?);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("Cannot bind {}: {e}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    println!("Dawg daemon listening at {}", socket.display());
    let bind = std::env::var("DAWG_WEB_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "DAWG_WEB_BIND must be an IP address and port")?;
    let (web_stop, web_shutdown) = tokio::sync::oneshot::channel::<()>();
    let web_manager = manager.clone();
    let mut web_task = tokio::spawn(web::serve(web_manager, bind, data_dir, async {
        let _ = web_shutdown.await;
    }));
    let network_manager = manager.clone();
    let monitor = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            network_manager.refresh_network().await;
        }
    });
    let mut shutdown_error = None;
    let mut web_finished = false;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            result = &mut web_task => {
                web_finished = true;
                shutdown_error = Some(match result { Ok(Err(error)) => error, Ok(Ok(())) => "Web listener stopped unexpectedly".into(), Err(error) => error.to_string() });
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        shutdown_error = Some(format!("IPC listener failed: {error}"));
                        break;
                    }
                };
                let manager = manager.clone();
                tokio::spawn(async move { if let Err(error) = handle_client(stream, manager).await { eprintln!("IPC connection: {error}"); } });
            }
        }
    }
    println!("Stopping managed servers…");
    drop(listener);
    let _ = web_stop.send(());
    monitor.abort();
    let _ = monitor.await;
    manager.shutdown().await;
    if !web_finished {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), web_task).await;
    }
    remove_socket(&socket);
    drop(lock);
    drop(data_lock);
    println!("Dawg daemon stopped.");
    if let Some(error) = shutdown_error {
        return Err(error);
    }
    Ok(())
}

fn remove_socket(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        eprintln!("Cannot remove {}: {error}", path.display());
    }
}

async fn handle_client(stream: UnixStream, manager: Arc<ServerManager>) -> Result<(), String> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut frame = Vec::new();
        let count = (&mut reader)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(());
        }
        if count > MAX_FRAME_BYTES {
            return Err("Request exceeds the 1 MiB frame limit".into());
        }
        let response = if frame.last() != Some(&b'\n') {
            Response::Error("Requests must end with a newline.".into())
        } else {
            match serde_json::from_slice::<Request>(&frame) {
                Ok(request) => manager.handle(request).await,
                Err(error) => Response::Error(format!("Invalid request: {error}")),
            }
        };
        let mut encoded = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
        if encoded.len() >= MAX_FRAME_BYTES {
            encoded = serde_json::to_vec(&Response::Error(
                "Response is too large; request a smaller range.".into(),
            ))
            .map_err(|e| e.to_string())?;
        }
        encoded.push(b'\n');
        write.write_all(&encoded).await.map_err(|e| e.to_string())?;
    }
}
