use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerType {
    Fabric,
    Vanilla,
    Paper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub path: String,
    pub server_type: ServerType,
    pub minecraft_version: String,
    pub loader_version: Option<String>,
    pub ram_mb: u32,
    pub java_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "New Server".to_string(),
            path: "".to_string(),
            server_type: ServerType::Fabric,
            minecraft_version: "1.20.4".to_string(),
            loader_version: None,
            ram_mb: 4096,
            java_path: "java".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert_eq!(config.name, "New Server");
        assert_eq!(config.ram_mb, 4096);
        assert_eq!(config.java_path, "java");
        assert_eq!(config.minecraft_version, "1.20.4");
        // default to fabric
        assert!(matches!(config.server_type, ServerType::Fabric));
    }
}
