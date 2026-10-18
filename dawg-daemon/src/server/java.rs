use std::process::Command;

pub fn get_java_version(java_path: &str) -> Option<String> {
    let output = Command::new(java_path)
        .arg("-version")
        .output()
        .ok()?;

    if output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(line) = stderr.lines().next() {
            return Some(line.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_java_version_bad_path() {
        // test non existent path
        let version = get_java_version("/invalid/path/to/java999");
        assert!(version.is_none());
    }
}
