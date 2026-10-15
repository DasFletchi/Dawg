use std::collections::HashMap;
use crate::server::config::ServerConfig;
use crate::server::process::ServerProcess;

pub struct ServerManager {
    servers: HashMap<String, ServerProcess>,
}

impl ServerManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub fn add_server(&mut self, config: ServerConfig) {
        let process = ServerProcess::new(config.clone());
        self.servers.insert(config.id, process);
    }

    pub async fn start_server(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(process) = self.servers.get_mut(id) {
            process.start().await?;
            Ok(())
        } else {
            Err("Server not found".into())
        }
    }

    pub async fn stop_server(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(process) = self.servers.get_mut(id) {
            process.stop().await?;
            Ok(())
        } else {
            Err("Server not found".into())
        }
    }
}
