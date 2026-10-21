pub use dawg_core::{ServerConfig, ServerKind, ServerStatus};

pub fn validate(config: &ServerConfig) -> Result<(), String> {
    if config.name.trim().is_empty()
        || config.name.chars().count() > 80
        || config.name.chars().any(char::is_control)
    {
        return Err("Server name must contain 1–80 printable characters.".into());
    }
    if config.memory_mb < 512 || config.memory_mb > 262144 {
        return Err("Memory must be between 512 and 262144 MB.".into());
    }
    if config.port < 1024 {
        return Err("Use a server port between 1024 and 65535.".into());
    }
    if config.minecraft_version.is_empty()
        || config.minecraft_version.len() > 64
        || !config
            .minecraft_version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    {
        return Err("Enter a Minecraft version such as 1.21.11.".into());
    }
    if config.java_path.trim().is_empty() || config.java_path.contains(['\0', '\n', '\r']) {
        return Err("Enter a Java executable, without command-line arguments.".into());
    }
    config.network.validate()?;
    Ok(())
}
