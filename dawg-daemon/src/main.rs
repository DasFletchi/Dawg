pub mod ipc;
pub mod metadata;
pub mod monitoring;
pub mod server;

use crate::ipc::message::{DaemonRequest, DaemonResponse};
use crate::server::manager::ServerManager;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::Mutex;

fn get_socket_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/run/user/{}/dawg.sock", uid)
}

#[tokio::main]
async fn main() {
    println!("Dawg Daemon started!");

    let socket_path = get_socket_path();
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind to socket {}: {}", socket_path, e);
            return;
        }
    };

    let manager = Arc::new(Mutex::new(ServerManager::new()));

    let default_config = server::config::ServerConfig {
        id: "1".to_string(),
        ..Default::default()
    };
    manager.lock().await.add_server(default_config);

    println!("Listening on Unix socket: {}", socket_path);

    let socket_path_cleanup = socket_path.clone();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("Received SIGINT, shutting down Dawg Daemon...");
        }
        _ = async {
            if let Some(sig) = &mut sigterm {
                sig.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            println!("Received SIGTERM, shutting down Dawg Daemon...");
        }
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((mut socket, _addr)) => {
                        let manager_clone = manager.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0; 4096];
                            loop {
                                match socket.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        let req_str = String::from_utf8_lossy(&buf[..n]);
                                        if let Ok(req) = serde_json::from_str::<DaemonRequest>(&req_str) {
                                            let mut mgr = manager_clone.lock().await;
                                            let response = match req {
                                                DaemonRequest::StartServer(id) => {
                                                    match mgr.start_server(&id).await {
                                                        Ok(_) => DaemonResponse::Ok,
                                                        Err(e) => DaemonResponse::Error(e.to_string()),
                                                    }
                                                }
                                                DaemonRequest::StopServer(id) => {
                                                    match mgr.stop_server(&id).await {
                                                        Ok(_) => DaemonResponse::Ok,
                                                        Err(e) => DaemonResponse::Error(e.to_string()),
                                                    }
                                                }
                                                DaemonRequest::ListServers => {
                                                    DaemonResponse::ServerList(vec!["1".to_string()])
                                                }
                                                DaemonRequest::GetStatus(_) => {
                                                    DaemonResponse::ServerStatus(
                                                        "1".to_string(),
                                                        "Stopped".to_string(),
                                                    )
                                                }
                                            };

                                            if let Ok(res_str) = serde_json::to_string(&response) {
                                                let _ = socket.write_all(res_str.as_bytes()).await;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Socket read error: {}", e);
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Accept failed: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        } => {}
    }

    let _ = std::fs::remove_file(&socket_path_cleanup);
    println!("Dawg Daemon stopped.");
}
