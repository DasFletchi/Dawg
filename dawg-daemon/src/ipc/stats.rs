use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStatsMessage {
    pub server_id: String,
    pub cpu_usage: f32,
    pub memory_bytes: u64,
}
