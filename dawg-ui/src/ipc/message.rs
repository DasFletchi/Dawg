use serde::{Deserialize, Serialize};
use crate::ipc::stats::ProcessStatsMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonRequest {
    StartServer(String),
    StopServer(String),
    GetStatus(String),
    ListServers,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    Ok,
    Error(String),
    ServerStatus(String, String), // id, status_string
    ServerList(Vec<String>), // list of ids
    LogLine(String, String), // id, line
    Stats(ProcessStatsMessage),
}
