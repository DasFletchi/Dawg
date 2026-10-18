use crate::ipc::stats::ProcessStatsMessage;
use serde::{Deserialize, Serialize};

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
    ServerStatus(String, String),
    ServerList(Vec<String>),
    LogLine(String, String),
    Stats(ProcessStatsMessage),
}
