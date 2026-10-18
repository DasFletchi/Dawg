use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::server::config::{ServerConfig, ServerStatus};

pub struct ServerProcess {
    config: ServerConfig,
    status: Arc<Mutex<ServerStatus>>,
    tx_input: Option<mpsc::Sender<String>>,
    // We would also have a log buffer here to broadcast to UI
}

impl ServerProcess {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            status: Arc::new(Mutex::new(ServerStatus::Stopped)),
            tx_input: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut status = self.status.lock().await;
        if *status != ServerStatus::Stopped && *status != ServerStatus::Crashed {
            return Err("Server is already running or starting".into());
        }
        *status = ServerStatus::Starting;
        drop(status);

        let mut cmd = Command::new(&self.config.java_path);

        // Setup arguments correctly to avoid shell injection
        cmd.arg(format!("-Xmx{}M", self.config.ram_mb));
        cmd.arg(format!("-Xms{}M", self.config.ram_mb / 2));
        cmd.arg("-jar");

        let jar_name = match self.config.server_type {
            crate::server::config::ServerType::Fabric => "fabric-server-launch.jar",
            crate::server::config::ServerType::Paper => "paper.jar",
            crate::server::config::ServerType::Vanilla => "server.jar",
        };
        cmd.arg(jar_name);
        cmd.arg("nogui");

        cmd.current_dir(&self.config.path);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                *self.status.lock().await = ServerStatus::Crashed;
                return Err(e.into());
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdin = child.stdin.take().unwrap();

        let (tx, mut rx) = mpsc::channel::<String>(32);
        self.tx_input = Some(tx);

        let status_ref = self.status.clone();

        // Spawn background task to monitor process and handle IO
        tokio::spawn(async move {
            *status_ref.lock().await = ServerStatus::Running;

            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut stdin_writer = stdin;

            loop {
                tokio::select! {
                    Ok(Some(line)) = stdout_reader.next_line() => {
                        println!("[STDOUT] {}", line); // TODO: Broadcast to UI
                    }
                    Ok(Some(line)) = stderr_reader.next_line() => {
                        eprintln!("[STDERR] {}", line); // TODO: Broadcast to UI
                    }
                    Some(cmd) = rx.recv() => {
                        if let Err(e) = stdin_writer.write_all(format!("{}\n", cmd).as_bytes()).await {
                            eprintln!("Failed to write to stdin: {}", e);
                        }
                    }
                    status = child.wait() => {
                        let exit_status = status.unwrap();
                        let mut state = status_ref.lock().await;
                        if exit_status.success() {
                            *state = ServerStatus::Stopped;
                        } else {
                            *state = ServerStatus::Crashed;
                        }
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut status = self.status.lock().await;
        if *status == ServerStatus::Running {
            *status = ServerStatus::Stopping;
            if let Some(tx) = &self.tx_input {
                tx.send("stop".to_string()).await?;
            }
        }
        Ok(())
    }

    pub async fn send_command(&mut self, cmd: String) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = &self.tx_input {
            tx.send(cmd).await?;
        }
        Ok(())
    }

    pub async fn get_status(&self) -> ServerStatus {
        self.status.lock().await.clone()
    }
}
