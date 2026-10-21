//! Shared, newline-delimited JSON protocol for the local Dawg desktop and daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod network;
pub use network::{ForwardingStatus, NetworkConfig, NetworkStatus, format_address, is_public_ip};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ServerKind {
    Vanilla,
    Paper,
    #[default]
    Fabric,
    Forge,
}

impl std::fmt::Display for ServerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Vanilla => "Vanilla",
            Self::Paper => "Paper",
            Self::Fabric => "Fabric",
            Self::Forge => "Forge",
        })
    }
}

impl ServerKind {
    pub const ALL: [Self; 4] = [Self::Vanilla, Self::Paper, Self::Fabric, Self::Forge];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ServerStatus {
    #[default]
    Stopped,
    Installing,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl ServerStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

impl std::fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "Offline",
            Self::Installing => "Installing",
            Self::Starting => "Starting",
            Self::Running => "Online",
            Self::Stopping => "Stopping",
            Self::Crashed => "Crashed",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub directory: String,
    pub kind: ServerKind,
    pub minecraft_version: String,
    pub memory_mb: u32,
    pub port: u16,
    pub java_path: String,
    pub eula_accepted: bool,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateServer {
    pub name: String,
    pub kind: ServerKind,
    pub minecraft_version: String,
    pub memory_mb: u32,
    pub port: u16,
    pub java_path: String,
    pub eula_accepted: bool,
    /// Optional local JAR copied into this server's managed directory.
    #[serde(default)]
    pub import_jar: Option<String>,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub config: ServerConfig,
    pub status: ServerStatus,
    pub installed: bool,
    pub pid: Option<u32>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub uptime_seconds: Option<u64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub network: NetworkStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostStats {
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub protocol_version: u32,
    pub data_directory: String,
    pub host: HostStats,
    pub servers: Vec<ServerSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VersionChannel {
    Release,
    Snapshot,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftVersion {
    pub id: String,
    pub channel: VersionChannel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionCatalog {
    pub kind: ServerKind,
    /// Official upstream order, newest first, restricted to the chosen loader.
    pub versions: Vec<MinecraftVersion>,
    pub latest_release: Option<String>,
    pub latest_snapshot: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Snapshot,
    VersionCatalog {
        kind: ServerKind,
    },
    CreateServer(CreateServer),
    UpdateServer(ServerConfig),
    InstallServer(String),
    StartServer(String),
    StopServer(String),
    RestartServer(String),
    SendCommand {
        id: String,
        command: String,
    },
    Logs {
        id: String,
        after: u64,
    },
    GetProperties(String),
    SaveProperties {
        id: String,
        content: String,
    },
    BackupServer(String),
    /// Unregister only. Server files are retained on disk.
    RemoveServer(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error(String),
    Snapshot(DashboardSnapshot),
    Server(Box<ServerSnapshot>),
    VersionCatalog(VersionCatalog),
    Logs { id: String, entries: Vec<LogEntry> },
    Properties { id: String, content: String },
    BackupCreated { path: String },
    Removed(String),
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("DAWG_DATA_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join("dawg");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".local/share/dawg")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DAWG_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join("dawg.sock");
    }
    // Same identity as the data directory, including when its location is overridden.
    data_dir().join("run/dawg.sock")
}
