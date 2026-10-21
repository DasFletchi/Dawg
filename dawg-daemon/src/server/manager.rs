use super::{
    config::validate,
    process::{Runtime, ServerProcess},
};
use dawg_core::{
    CreateServer, DashboardSnapshot, HostStats, PROTOCOL_VERSION, Request, Response, ServerConfig,
    ServerStatus,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};

pub struct ServerManager {
    data_dir: PathBuf,
    servers: Mutex<BTreeMap<String, Arc<ServerProcess>>>,
    system: Mutex<System>,
    shutting_down: AtomicBool,
    network: crate::network::NetworkService,
    tunnels: crate::tunnel::TunnelService,
}

impl ServerManager {
    pub async fn load(data_dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(data_dir.join("servers"))
            .await
            .map_err(io_error)?;
        let data_dir = fs::canonicalize(data_dir).await.map_err(io_error)?;
        let path = data_dir.join("servers.json");
        let configs: Vec<ServerConfig> = match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                format!(
                    "Cannot read {}: {e}. The file was not changed.",
                    path.display()
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_error(e)),
        };
        let mut servers = BTreeMap::new();
        let mut relocated = false;
        for mut config in configs {
            validate(&config)?;
            uuid::Uuid::parse_str(&config.id)
                .map_err(|_| "Invalid server ID in saved configuration")?;
            let expected = data_dir.join("servers").join(&config.id);
            let managed = expected.to_string_lossy().to_string();
            if config.directory != managed {
                relocated = true;
                config.directory = managed;
            }
            if servers
                .values()
                .any(|s: &Arc<ServerProcess>| s.config.port == config.port)
            {
                return Err(
                    "Saved servers use the same port. Correct servers.json before restarting."
                        .into(),
                );
            }
            if servers
                .insert(config.id.clone(), Arc::new(ServerProcess::new(config)))
                .is_some()
            {
                return Err("Duplicate server IDs in saved configuration.".into());
            }
        }
        if relocated {
            let configs: Vec<_> = servers
                .values()
                .map(|server| server.config.clone())
                .collect();
            atomic_write(
                &data_dir.join("servers.json"),
                &serde_json::to_vec_pretty(&configs).map_err(|error| error.to_string())?,
            )
            .await?;
        }
        Ok(Self {
            data_dir,
            servers: Mutex::new(servers),
            system: Mutex::new(System::new_all()),
            shutting_down: AtomicBool::new(false),
            network: crate::network::NetworkService::new(),
            tunnels: crate::tunnel::TunnelService::new(),
        })
    }

    pub async fn handle(&self, request: Request) -> Response {
        match self.execute(request).await {
            Ok(response) => response,
            Err(error) => Response::Error(error),
        }
    }

    async fn execute(&self, request: Request) -> Result<Response, String> {
        self.ensure_open()?;
        match request {
            Request::Snapshot => self.snapshot().await,
            Request::VersionCatalog { kind } => Ok(Response::VersionCatalog(
                crate::metadata::catalog::fetch(kind).await?,
            )),
            Request::CreateServer(input) => self.create(input).await,
            Request::UpdateServer(config) => self.update(config).await,
            Request::StartServer(id) => {
                let servers = self.servers.lock().await;
                self.ensure_open()?;
                let process = servers.get(&id).cloned().ok_or("Server not found")?;
                let mut state = process.runtime.lock().await;
                if state.busy
                    || state.status.is_active()
                    || state.status == ServerStatus::Installing
                {
                    return Err("Server is busy. Wait for current work before starting.".into());
                }
                state.busy = true;
                let reservation = FileOperationGuard {
                    config: process.config.clone(),
                    runtime: process.runtime.clone(),
                };
                drop(state);
                drop(servers);
                let result = process.start_reserved().await;
                drop(reservation);
                result?;
                Ok(Response::Ok)
            }
            Request::StopServer(id) => {
                self.get(&id).await?.stop().await?;
                Ok(Response::Ok)
            }
            Request::RestartServer(id) => {
                let servers = self.servers.lock().await;
                self.ensure_open()?;
                let process = servers.get(&id).cloned().ok_or("Server not found")?;
                process.restart().await?;
                drop(servers);
                Ok(Response::Ok)
            }
            Request::InstallServer(id) => {
                let servers = self.servers.lock().await;
                self.ensure_open()?;
                let process = servers.get(&id).cloned().ok_or("Server not found")?;
                if !process.config.eula_accepted {
                    return Err("Accept the Minecraft EULA in settings before installing.".into());
                }
                {
                    let mut state = process.runtime.lock().await;
                    if state.busy
                        || state.status.is_active()
                        || state.status == ServerStatus::Installing
                    {
                        return Err("Stop this server and wait for current work to finish before installing.".into());
                    }
                    state.status = ServerStatus::Installing;
                    state.last_error = None;
                }
                drop(servers);
                tokio::spawn(async move {
                    process
                        .log(
                            "system",
                            format!(
                                "Downloading {} {} from its official distribution…",
                                process.config.kind, process.config.minecraft_version
                            ),
                        )
                        .await;
                    let result = async {
                        let java = super::java::resolve_for_install(
                            &process.config.java_path,
                            &process.config.minecraft_version,
                        )
                        .await?;
                        crate::metadata::installer::install(
                            &process.config.kind,
                            &process.config.minecraft_version,
                            Path::new(&process.config.directory),
                            &java.to_string_lossy(),
                        )
                        .await
                    }
                    .await;
                    match result {
                        Ok(()) => {
                            let mut state = process.runtime.lock().await;
                            state.status = ServerStatus::Stopped;
                            state.last_error = None;
                            drop(state);
                            process
                                .log("system", "Server installed. You can start it now.")
                                .await;
                        }
                        Err(error) => process.fail(error).await,
                    }
                });
                Ok(Response::Ok)
            }
            Request::SendCommand { id, command } => {
                self.get(&id).await?.command(command).await?;
                Ok(Response::Ok)
            }
            Request::Logs { id, after } => {
                let process = self.get(&id).await?;
                let state = process.runtime.lock().await;
                Ok(Response::Logs {
                    id,
                    entries: state
                        .logs
                        .iter()
                        .filter(|entry| entry.seq > after)
                        .take(64)
                        .cloned()
                        .collect(),
                })
            }
            Request::GetProperties(id) => {
                let process = self.get(&id).await?;
                let directory = process.config.directory.clone();
                let content = tokio::task::spawn_blocking(move || read_properties(&directory))
                    .await
                    .map_err(|_| "Properties read was interrupted".to_string())??;
                Ok(Response::Properties { id, content })
            }
            Request::SaveProperties { id, content } => {
                if content.len() > 65536 || content.contains('\0') {
                    return Err("Properties must be a text file smaller than 64 KiB.".into());
                }
                let servers = self.servers.lock().await;
                self.ensure_open()?;
                let process = servers.get(&id).cloned().ok_or("Server not found")?;
                let state = process.runtime.lock().await;
                if state.busy
                    || state.status.is_active()
                    || state.status == ServerStatus::Installing
                {
                    return Err("Stop the server before editing its properties.".into());
                }
                if let Some(port) = property_value(&content, "server-port")
                    && port != process.config.port.to_string()
                {
                    return Err("Change the server port in server settings so Dawg can check for port conflicts.".into());
                }
                let normalized = normalize_properties(&content, process.config.port);
                atomic_write(
                    &Path::new(&process.config.directory).join("server.properties"),
                    normalized.as_bytes(),
                )
                .await?;
                Ok(Response::Ok)
            }
            Request::BackupServer(id) => self.backup(&id).await,
            Request::RemoveServer(id) => {
                let mut servers = self.servers.lock().await;
                self.ensure_open()?;
                let process = servers.get(&id).ok_or("Server not found")?;
                let state = process.runtime.lock().await;
                if state.busy
                    || state.status.is_active()
                    || state.status == ServerStatus::Installing
                {
                    return Err(
                        "Stop the server and wait for current work to finish before removing it."
                            .into(),
                    );
                }
                let configs: Vec<_> = servers
                    .values()
                    .filter(|s| s.config.id != id)
                    .map(|s| s.config.clone())
                    .collect();
                self.persist(&configs).await?;
                drop(state);
                servers.remove(&id);
                Ok(Response::Removed(id))
            }
        }
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            Err("The daemon is shutting down.".into())
        } else {
            Ok(())
        }
    }

    async fn get(&self, id: &str) -> Result<Arc<ServerProcess>, String> {
        self.servers
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| "Server not found".into())
    }

    async fn snapshot(&self) -> Result<Response, String> {
        let processes: Vec<_> = self.servers.lock().await.values().cloned().collect();
        let mut snapshots = Vec::with_capacity(processes.len());
        for process in processes {
            let mut snapshot = process.snapshot().await;
            snapshot.network = self
                .tunnels
                .status(&snapshot.config)
                .await
                .unwrap_or(self.network.status(&snapshot.config).await);
            snapshots.push(snapshot);
        }
        let mut system = self.system.lock().await;
        system.refresh_cpu_usage();
        system.refresh_memory();
        let pids: Vec<_> = snapshots
            .iter()
            .filter_map(|s| s.pid)
            .map(Pid::from_u32)
            .collect();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        for snapshot in &mut snapshots {
            if let Some(pid) = snapshot.pid
                && let Some(process) = system.process(Pid::from_u32(pid))
            {
                snapshot.cpu_percent = process.cpu_usage();
                snapshot.memory_bytes = process.memory();
            }
        }
        snapshots.sort_by(|a, b| {
            a.config
                .name
                .to_lowercase()
                .cmp(&b.config.name.to_lowercase())
        });
        Ok(Response::Snapshot(DashboardSnapshot {
            protocol_version: PROTOCOL_VERSION,
            data_directory: self.data_dir.to_string_lossy().into(),
            host: HostStats {
                cpu_percent: system.global_cpu_usage(),
                memory_used_bytes: system.used_memory(),
                memory_total_bytes: system.total_memory(),
            },
            servers: snapshots,
        }))
    }

    async fn create(&self, input: CreateServer) -> Result<Response, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let directory = self.data_dir.join("servers").join(&id);
        let config = ServerConfig {
            id: id.clone(),
            name: input.name.trim().into(),
            directory: directory.to_string_lossy().into(),
            kind: input.kind,
            minecraft_version: input.minecraft_version,
            memory_mb: input.memory_mb,
            port: input.port,
            java_path: input.java_path.trim().into(),
            eula_accepted: input.eula_accepted,
            network: input.network,
        };
        validate(&config)?;
        let mut servers = self.servers.lock().await;
        self.ensure_open()?;
        check_port(&servers, &config)?;
        fs::create_dir_all(&directory).await.map_err(io_error)?;
        if let Some(source) = input.import_jar {
            if config.kind == dawg_core::ServerKind::Forge {
                return Err(
                    "Use the official Forge installer, then upload Forge mods through Files."
                        .into(),
                );
            }
            let metadata = fs::metadata(&source)
                .await
                .map_err(|e| format!("Cannot read the selected JAR: {e}"))?;
            if !metadata.is_file() || metadata.len() > 1024 * 1024 * 1024 {
                return Err("Select a regular server JAR smaller than 1 GiB.".into());
            }
            crate::metadata::installer::validate_jar(Path::new(&source))?;
            fs::copy(&source, directory.join("server.jar"))
                .await
                .map_err(io_error)?;
        }
        write_runtime_files(&config).await?;
        let mut configs: Vec<_> = servers.values().map(|s| s.config.clone()).collect();
        configs.push(config.clone());
        self.persist(&configs).await?;
        let process = Arc::new(ServerProcess::new(config));
        let snapshot = process.snapshot().await;
        servers.insert(id, process);
        Ok(Response::Server(Box::new(snapshot)))
    }

    async fn update(&self, config: ServerConfig) -> Result<Response, String> {
        validate(&config)?;
        let mut servers = self.servers.lock().await;
        self.ensure_open()?;
        let process = servers.get(&config.id).ok_or("Server not found")?;
        if config.directory != process.config.directory {
            return Err("The managed server directory cannot be changed.".into());
        }
        let state = process.runtime.lock().await;
        if state.busy || state.status.is_active() || state.status == ServerStatus::Installing {
            return Err("Stop the server before changing its settings.".into());
        }
        check_port(&servers, &config)?;
        if (config.kind != process.config.kind
            || config.minecraft_version != process.config.minecraft_version)
            && crate::metadata::installer::is_installed(
                Path::new(&config.directory),
                process.config.kind,
            )
        {
            return Err("The installed software and version cannot be relabelled. Create a new server or import a new JAR instead.".into());
        }
        let previous_files = read_runtime_files(&config).await?;
        let configs: Vec<_> = servers
            .values()
            .map(|s| {
                if s.config.id == config.id {
                    config.clone()
                } else {
                    s.config.clone()
                }
            })
            .collect();
        let save_result = async {
            write_runtime_files(&config).await?;
            self.persist(&configs).await
        }
        .await;
        if let Err(error) = save_result {
            let rollback = restore_runtime_files(previous_files).await;
            return Err(match rollback {
                Ok(()) => error,
                Err(rollback_error) => format!(
                    "{error}; restoring previous runtime files also failed: {rollback_error}"
                ),
            });
        }
        let new_process = Arc::new(ServerProcess::new(config));
        {
            let mut new_state = new_process.runtime.lock().await;
            new_state.logs = state.logs.clone();
            new_state.next_seq = state.next_seq;
        }
        let snapshot = new_process.snapshot().await;
        drop(state);
        servers.insert(new_process.config.id.clone(), new_process);
        Ok(Response::Server(Box::new(snapshot)))
    }

    async fn persist(&self, configs: &[ServerConfig]) -> Result<(), String> {
        atomic_write(
            &self.data_dir.join("servers.json"),
            &serde_json::to_vec_pretty(configs).map_err(|e| e.to_string())?,
        )
        .await
    }

    async fn backup(&self, id: &str) -> Result<Response, String> {
        let servers = self.servers.lock().await;
        self.ensure_open()?;
        let process = servers.get(id).cloned().ok_or("Server not found")?;
        {
            let mut state = process.runtime.lock().await;
            if state.busy || state.status.is_active() || state.status == ServerStatus::Installing {
                return Err("Stop the server before making a consistent backup.".into());
            }
            state.busy = true;
        }
        drop(servers);
        let result = async {
            let directory = self.data_dir.join("backups");
            fs::create_dir_all(&directory).await.map_err(io_error)?;
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let path = directory.join(format!(
                "{id}-{epoch}-{}.tar.gz",
                uuid::Uuid::new_v4().simple()
            ));
            let temporary = path.with_extension("tar.gz.partial");
            process
                .log("system", "Creating a backup of the stopped server…")
                .await;
            let output = tokio::process::Command::new("tar")
                .arg("-czf")
                .arg(&temporary)
                .arg("-C")
                .arg(&process.config.directory)
                .arg(".")
                .output()
                .await
                .map_err(|e| format!("Cannot launch tar for backup: {e}"))?;
            if !output.status.success() {
                let _ = fs::remove_file(&temporary).await;
                return Err(format!(
                    "Backup failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            fs::rename(&temporary, &path).await.map_err(io_error)?;
            process
                .log("system", format!("Backup saved: {}", path.display()))
                .await;
            Ok(Response::BackupCreated {
                path: path.to_string_lossy().into(),
            })
        }
        .await;
        process.runtime.lock().await.busy = false;
        result
    }

    /// Cached status and the immutable managed root used by the file API.
    pub async fn file_server(&self, id: &str) -> Result<dawg_core::ServerSnapshot, String> {
        self.ensure_open()?;
        Ok(self.get(id).await?.snapshot().await)
    }

    /// Reserve offline file writes without blocking console polling during uploads.
    pub async fn begin_file_operation(&self, id: &str) -> Result<FileOperationGuard, String> {
        let servers = self.servers.lock().await;
        self.ensure_open()?;
        let process = servers.get(id).ok_or("Server not found")?;
        let mut state = process.runtime.lock().await;
        if state.busy || state.status.is_active() || state.status == ServerStatus::Installing {
            return Err("Stop the server and wait for current work before changing files.".into());
        }
        state.busy = true;
        Ok(FileOperationGuard {
            config: process.config.clone(),
            runtime: process.runtime.clone(),
        })
    }

    /// Called only by the background worker; snapshots never wait for routers or providers.
    pub async fn refresh_network(&self) {
        let processes: Vec<_> = self.servers.lock().await.values().cloned().collect();
        let mut servers = Vec::with_capacity(processes.len());
        for process in processes {
            let status = process.runtime.lock().await.status;
            servers.push((process.config.clone(), status));
        }
        tokio::join!(
            self.network.update(servers.clone()),
            self.tunnels.update(servers)
        );
    }

    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let processes: Vec<_> = self.servers.lock().await.values().cloned().collect();
        for process in &processes {
            process.shutting_down.store(true, Ordering::SeqCst);
            let _ = process.stop().await;
        }
        for process in processes {
            if !process.wait_stopped().await {
                eprintln!(
                    "Server {} did not exit after forced termination.",
                    process.config.name
                );
            }
        }
        tokio::join!(self.network.shutdown(), self.tunnels.shutdown());
    }
}

pub struct FileOperationGuard {
    pub config: ServerConfig,
    runtime: Arc<Mutex<Runtime>>,
}
impl Drop for FileOperationGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.runtime.try_lock() {
            runtime.busy = false;
        } else {
            let runtime = self.runtime.clone();
            tokio::spawn(async move {
                runtime.lock().await.busy = false;
            });
        }
    }
}

fn check_port(
    servers: &BTreeMap<String, Arc<ServerProcess>>,
    config: &ServerConfig,
) -> Result<(), String> {
    if servers
        .values()
        .any(|s| s.config.id != config.id && s.config.port == config.port)
    {
        return Err(format!(
            "Port {} is already assigned to another server.",
            config.port
        ));
    }
    if config.network.upnp_enabled
        && servers.values().any(|s| {
            s.config.id != config.id
                && s.config.network.upnp_enabled
                && s.config.network.external_port(s.config.port)
                    == config.network.external_port(config.port)
        })
    {
        return Err("This external TCP port is already assigned to another server.".into());
    }
    Ok(())
}
fn read_managed_bytes(directory: &Path, name: &str) -> std::io::Result<Vec<u8>> {
    use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
    use std::io::Read;
    let root = Dir::open_ambient_dir(directory, cap_std::ambient_authority())?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = root.open_with(name, &options)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Managed text must be a regular file smaller than 64 KiB.",
        ));
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Managed text exceeds 64 KiB.",
        ));
    }
    Ok(bytes)
}
fn read_properties(directory: &str) -> Result<String, String> {
    let bytes = read_managed_bytes(Path::new(directory), "server.properties").map_err(io_error)?;
    String::from_utf8(bytes).map_err(|_| "Properties must be UTF-8 text.".into())
}

fn property_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| name.trim() == key)
        .map(|(_, value)| value.trim())
        .next_back()
}

async fn write_runtime_files(config: &ServerConfig) -> Result<(), String> {
    let directory = Path::new(&config.directory);
    atomic_write(
        &directory.join("eula.txt"),
        format!(
            "# Accepted explicitly in Dawg. https://aka.ms/MinecraftEULA\neula={}\n",
            config.eula_accepted
        )
        .as_bytes(),
    )
    .await?;
    let path = directory.join("server.properties");
    let previous = match read_managed_bytes(directory, "server.properties") {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| "Properties must be UTF-8 text.")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "motd=A Minecraft server managed by Dawg\nonline-mode=true\nenable-command-block=false\n".into(),
        Err(e) => return Err(io_error(e)),
    };
    atomic_write(
        &path,
        normalize_properties(&previous, config.port).as_bytes(),
    )
    .await
}

fn normalize_properties(content: &str, port: u16) -> String {
    let lines: Vec<_> = content
        .lines()
        .filter(|line| {
            !line
                .split_once('=')
                .is_some_and(|(name, _)| name.trim() == "server-port")
                && *line != "# Dawg manages this port; change it in Settings."
        })
        .collect();
    // This comment also terminates a continued final line before our assignment.
    format!(
        "{}\n# Dawg manages this port; change it in Settings.\nserver-port={port}\n",
        lines.join("\n")
    )
}

type SavedFiles = Vec<(PathBuf, Option<Vec<u8>>)>;
async fn read_runtime_files(config: &ServerConfig) -> Result<SavedFiles, String> {
    let mut saved = Vec::new();
    for name in ["eula.txt", "server.properties"] {
        let path = Path::new(&config.directory).join(name);
        let bytes = match read_managed_bytes(Path::new(&config.directory), name) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(error)),
        };
        saved.push((path, bytes));
    }
    Ok(saved)
}
async fn restore_runtime_files(saved: SavedFiles) -> Result<(), String> {
    let mut errors = Vec::new();
    for (path, bytes) in saved {
        let result = match bytes {
            Some(bytes) => atomic_write(&path, &bytes).await,
            None => match fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error(error)),
            },
        };
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub async fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(io_error)?;
    let result = async {
        file.write_all(content).await.map_err(io_error)?;
        file.sync_all().await.map_err(io_error)?;
        fs::rename(&temporary, path).await.map_err(io_error)
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}
fn io_error(error: std::io::Error) -> String {
    error.to_string()
}
