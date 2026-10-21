use dawg_core::{LogEntry, ServerConfig, ServerSnapshot, ServerStatus};
use std::{
    collections::VecDeque,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, mpsc},
};

const MAX_LOG_LINES: usize = 2000;
const MAX_LOG_LINE: usize = 8192;

enum Control {
    Command(String),
    Stop,
    Kill,
}

pub struct Runtime {
    pub status: ServerStatus,
    pub pid: Option<u32>,
    pub started: Option<Instant>,
    pub last_error: Option<String>,
    pub logs: VecDeque<LogEntry>,
    pub busy: bool,
    pub next_seq: u64,
    input: Option<mpsc::Sender<Control>>,
}

pub struct ServerProcess {
    pub config: ServerConfig,
    pub runtime: Arc<Mutex<Runtime>>,
    pub shutting_down: AtomicBool,
}

impl ServerProcess {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            shutting_down: AtomicBool::new(false),
            runtime: Arc::new(Mutex::new(Runtime {
                status: ServerStatus::Stopped,
                pid: None,
                started: None,
                last_error: None,
                logs: VecDeque::new(),
                busy: false,
                next_seq: 1,
                input: None,
            })),
        }
    }

    pub async fn log(&self, stream: &str, line: impl Into<String>) {
        append_log(&self.runtime, stream, line.into()).await;
    }

    pub async fn fail(&self, error: String) {
        let mut state = self.runtime.lock().await;
        state.status = ServerStatus::Crashed;
        state.last_error = Some(error.clone());
        drop(state);
        self.log("error", error).await;
    }

    pub async fn snapshot(&self) -> ServerSnapshot {
        let state = self.runtime.lock().await;
        ServerSnapshot {
            config: self.config.clone(),
            status: state.status,
            installed: crate::metadata::installer::is_installed(
                std::path::Path::new(&self.config.directory),
                self.config.kind,
            ),
            pid: state.pid,
            cpu_percent: 0.0,
            memory_bytes: 0,
            uptime_seconds: state.started.map(|t| t.elapsed().as_secs()),
            last_error: state.last_error.clone(),
            network: Default::default(),
        }
    }

    /// The manager reserves `busy` while it still holds the registry lock.
    pub async fn start_reserved(&self) -> Result<(), String> {
        self.start_inner(true).await
    }

    async fn start_inner(&self, reserved: bool) -> Result<(), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("The daemon is shutting down.".into());
        }
        if !self.config.eula_accepted {
            return Err("Accept the Minecraft EULA before starting this server.".into());
        }
        let directory = std::path::Path::new(&self.config.directory);
        if !crate::metadata::installer::is_installed(directory, self.config.kind) {
            return Err("Install server software before starting.".into());
        }
        // Runtime resolution can download a verified JRE. It must not hold
        // the registry or runtime locks while waiting for the network.
        let java = super::java::resolve(&self.config).await?;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("The daemon is shutting down.".into());
        }
        let mut state = self.runtime.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("The daemon is shutting down.".into());
        }
        if (!reserved && state.busy)
            || state.status.is_active()
            || state.status == ServerStatus::Installing
        {
            return Err("Server is busy. Stop it before starting again.".into());
        }
        if !self.config.eula_accepted {
            return Err("Accept the Minecraft EULA before starting this server.".into());
        }
        let directory = std::path::Path::new(&self.config.directory);
        if !crate::metadata::installer::is_installed(directory, self.config.kind) {
            return Err("Install a server JAR before starting.".into());
        }
        let launch_args = crate::metadata::installer::launch_args(directory, self.config.kind)?;
        let mut command = Command::new(&java);
        command
            .arg(format!("-Xms{}M", self.config.memory_mb.min(1024)))
            .arg(format!("-Xmx{}M", self.config.memory_mb))
            .args(launch_args)
            .arg("nogui")
            .current_dir(&self.config.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            let parent = unsafe { libc::getpid() };
            // Only async-signal-safe syscalls run between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        return Err(std::io::Error::other("Daemon exited while launching Java"));
                    }
                    Ok(())
                });
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!("Cannot launch Java ({}): {error}", self.config.java_path);
                state.status = ServerStatus::Crashed;
                state.last_error = Some(message.clone());
                drop(state);
                self.log("error", &message).await;
                return Err(message);
            }
        };
        let stdout = child.stdout.take().ok_or("Java stdout is unavailable")?;
        let stderr = child.stderr.take().ok_or("Java stderr is unavailable")?;
        let mut stdin = child.stdin.take().ok_or("Java stdin is unavailable")?;
        let (input, mut commands) = mpsc::channel(64);
        state.input = Some(input);
        state.pid = child.id();
        state.status = ServerStatus::Starting;
        state.started = Some(Instant::now());
        state.last_error = None;
        drop(state);
        self.log(
            "system",
            "Starting Java. Waiting for the server's ready message…",
        )
        .await;
        tokio::spawn(read_output(stdout, self.runtime.clone(), "stdout"));
        tokio::spawn(read_output(stderr, self.runtime.clone(), "stderr"));
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            let mut stop_deadline: Option<Instant> = None;
            let exit = loop {
                tokio::select! {
                    result = child.wait() => break result,
                    command = commands.recv() => match command {
                        Some(Control::Command(line)) => {
                            if let Err(error) = write_input(&mut stdin, format!("{line}\n").as_bytes()).await {
                                append_log(&runtime, "error", error).await;
                                let _ = child.start_kill();
                            }
                        }
                        Some(Control::Stop) => {
                            if let Err(error) = write_input(&mut stdin, b"stop\n").await { append_log(&runtime, "error", error).await; let _ = child.start_kill(); }
                            stop_deadline = Some(Instant::now() + Duration::from_secs(30));
                        }
                        Some(Control::Kill) | None => { let _ = child.start_kill(); }
                    },
                    _ = tokio::time::sleep(Duration::from_millis(200)), if stop_deadline.is_some() => {
                        if stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            append_log(&runtime, "system", "Graceful shutdown timed out after 30 seconds; terminating Java.".into()).await;
                            let _ = child.start_kill();
                            stop_deadline = None;
                        }
                    }
                }
            };
            let mut state = runtime.lock().await;
            let intentional = state.status == ServerStatus::Stopping;
            let success = exit.as_ref().is_ok_and(|s| s.success());
            state.status = if success || intentional {
                ServerStatus::Stopped
            } else {
                ServerStatus::Crashed
            };
            state.last_error = if success || intentional {
                None
            } else {
                Some(format!(
                    "Java exited unexpectedly: {}",
                    exit.map(|s| s.to_string())
                        .unwrap_or_else(|e| e.to_string())
                ))
            };
            state.pid = None;
            state.started = None;
            state.input = None;
            let message = state
                .last_error
                .clone()
                .unwrap_or_else(|| "Server stopped.".into());
            drop(state);
            append_log(
                &runtime,
                if success || intentional {
                    "system"
                } else {
                    "error"
                },
                message,
            )
            .await;
        });
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), String> {
        let mut state = self.runtime.lock().await;
        if state.status == ServerStatus::Installing {
            return Err("Wait for installation to finish.".into());
        }
        let input = state.input.clone();
        if input.is_none() {
            if state.busy && !self.shutting_down.load(Ordering::SeqCst) {
                return Err(
                    "Wait for preparation or the current file operation to finish before stopping."
                        .into(),
                );
            }
            return Ok(());
        }
        state.status = ServerStatus::Stopping;
        drop(state);
        input
            .unwrap()
            .send(Control::Stop)
            .await
            .map_err(|_| "Java has already exited.".to_owned())
    }

    pub async fn restart(self: &Arc<Self>) -> Result<(), String> {
        let needs_stop = {
            let mut state = self.runtime.lock().await;
            if state.busy || state.status == ServerStatus::Installing {
                return Err("Wait for current work to finish before restarting.".into());
            }
            state.busy = true;
            state.status.is_active()
        };
        if needs_stop && let Err(error) = self.stop().await {
            self.runtime.lock().await.busy = false;
            return Err(error);
        }
        let process = self.clone();
        tokio::spawn(async move {
            if !process.wait_stopped().await {
                process.runtime.lock().await.busy = false;
                process.log("error", "Cannot restart: the previous Java process has not exited after forced termination.").await;
                return;
            }
            if let Err(error) = process.start_inner(true).await {
                process.fail(error).await;
                process.runtime.lock().await.busy = false;
            } else {
                process.runtime.lock().await.busy = false;
            }
        });
        Ok(())
    }

    pub async fn command(&self, command: String) -> Result<(), String> {
        let command = command.trim().trim_start_matches('/').to_owned();
        if command.is_empty() || command.len() > 4096 || command.contains(['\n', '\r', '\0']) {
            return Err("Enter one command of at most 4096 characters.".into());
        }
        let state = self.runtime.lock().await;
        if !matches!(state.status, ServerStatus::Running | ServerStatus::Starting) {
            return Err("Start this server before sending commands.".into());
        }
        let input = state.input.clone().ok_or("Server stdin is unavailable")?;
        drop(state);
        input
            .send(Control::Command(command.clone()))
            .await
            .map_err(|_| "Java has already exited.".to_owned())?;
        self.log("command", format!("> {command}")).await;
        Ok(())
    }

    pub async fn wait_stopped(&self) -> bool {
        for _ in 0..175 {
            if !self.runtime.lock().await.status.is_active() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let input = self.runtime.lock().await.input.clone();
        if let Some(input) = input {
            let _ = tokio::time::timeout(Duration::from_secs(2), input.send(Control::Kill)).await;
        }
        for _ in 0..50 {
            if !self.runtime.lock().await.status.is_active() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }
}

async fn write_input(stdin: &mut tokio::process::ChildStdin, bytes: &[u8]) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(2), stdin.write_all(bytes))
        .await
        .map_err(|_| {
            "Java stopped reading console input; terminating the unresponsive process.".to_owned()
        })?
        .map_err(|error| format!("Cannot write console input: {error}"))
}

async fn append_log(runtime: &Arc<Mutex<Runtime>>, stream: &str, mut line: String) {
    if line.len() > MAX_LOG_LINE {
        let mut end = MAX_LOG_LINE;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str(" … [truncated]");
    }
    let mut state = runtime.lock().await;
    if state.status == ServerStatus::Starting && line.contains("Done (") {
        state.status = ServerStatus::Running;
    }
    let seq = state.next_seq;
    state.next_seq += 1;
    state.logs.push_back(LogEntry {
        seq,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        stream: stream.into(),
        line,
    });
    while state.logs.len() > MAX_LOG_LINES {
        state.logs.pop_front();
    }
}

async fn read_output(
    mut stream: impl AsyncRead + Unpin,
    runtime: Arc<Mutex<Runtime>>,
    source: &'static str,
) {
    let mut buffer = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                for byte in &buffer[..count] {
                    if *byte == b'\n' {
                        append_log(
                            &runtime,
                            source,
                            String::from_utf8_lossy(&line).trim_end_matches('\r').into(),
                        )
                        .await;
                        line.clear();
                    } else if line.len() < MAX_LOG_LINE {
                        line.push(*byte);
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        append_log(&runtime, source, String::from_utf8_lossy(&line).into()).await;
    }
}
