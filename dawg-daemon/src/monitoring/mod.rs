use serde::{Deserialize, Serialize};
use std::sync::Arc;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStats {
    pub cpu_usage: f32, // percentage
    pub memory_bytes: u64,
}

pub struct Monitor {
    system: Arc<Mutex<System>>,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            system: Arc::new(Mutex::new(System::new_all())),
        }
    }

    pub async fn get_stats(&self, pid: u32) -> Option<ProcessStats> {
        let mut sys = self.system.lock().await;

        let refresh_kind = ProcessRefreshKind::nothing().with_cpu().with_memory();

        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true, // remove dead processes
            refresh_kind,
        );

        if let Some(process) = sys.process(Pid::from_u32(pid)) {
            return Some(ProcessStats {
                cpu_usage: process.cpu_usage(),
                memory_bytes: process.memory(),
            });
        }
        None
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}
