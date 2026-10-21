use dawg_core::ServerConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
const MAX_METADATA: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
struct RuntimeProfile {
    java_major: u32,
}

#[derive(Serialize, Deserialize)]
struct VerifiedRuntime {
    java_major: u32,
    archive_sha256: String,
    java_sha256: String,
}

#[derive(Debug)]
struct Package {
    url: String,
    checksum: String,
    size: u64,
}

pub fn get_java_version(java_path: &str) -> Option<String> {
    let output = Command::new(java_path).arg("-version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr
        .lines()
        .next()
        .or_else(|| stdout.lines().next())
        .map(str::to_owned)
}

/// Imported JARs without an installer profile keep using local Java. Starting
/// never guesses a game version or fetches Minecraft metadata.
pub async fn resolve(config: &ServerConfig) -> Result<PathBuf, String> {
    let path = Path::new(&config.directory).join(".dawg-runtime.json");
    resolve_requirement(&config.java_path, read_requirement(&path).await?).await
}

async fn read_requirement(path: &Path) -> Result<Option<u32>, String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    match options.open(path).await {
        Ok(file) => {
            let metadata = file.metadata().await.map_err(|e| e.to_string())?;
            if !metadata.is_file() || metadata.len() > 4096 {
                return Err(
                    "The server Java requirement must be a regular file up to 4 KiB.".into(),
                );
            }
            let mut bytes = Vec::new();
            file.take(4097)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Some(parse_profile(&bytes)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Could not read the server Java requirement: {error}"
        )),
    }
}

pub async fn resolve_for_install(java_path: &str, version: &str) -> Result<PathBuf, String> {
    resolve_requirement(
        java_path,
        crate::metadata::catalog::required_java(version).await?,
    )
    .await
}

fn parse_profile(bytes: &[u8]) -> Result<u32, String> {
    if bytes.len() > 4096 {
        return Err("The server Java requirement file is too large.".into());
    }
    let profile: RuntimeProfile = serde_json::from_slice(bytes).map_err(
        |_| "The server Java requirement file is invalid. Reinstall the server to restore it.",
    )?;
    if !(8..=99).contains(&profile.java_major) {
        return Err("Unsupported Java major version in the server requirement file.".into());
    }
    Ok(profile.java_major)
}

fn java_major(version: &str) -> Option<u32> {
    let quoted = version.split('"').nth(1).unwrap_or(version);
    let token = quoted
        .split_whitespace()
        .find(|part| part.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut components = token.split(|c: char| !c.is_ascii_digit());
    let first: u32 = components.next()?.parse().ok()?;
    if first == 1 {
        components.next()?.parse().ok()
    } else {
        Some(first)
    }
}

async fn probe(path: &Path) -> Option<u32> {
    let mut command = tokio::process::Command::new(path);
    command.arg("-version").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    java_major(&String::from_utf8_lossy(&output.stderr))
        .or_else(|| java_major(&String::from_utf8_lossy(&output.stdout)))
}

async fn local_java(required: Option<u32>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        candidates.push(PathBuf::from(home).join("bin/java"));
    }
    candidates.push(PathBuf::from("java"));
    for root in ["/usr/lib/jvm", "/usr/java", "/opt/java"] {
        if let Ok(mut entries) = tokio::fs::read_dir(root).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                candidates.push(entry.path().join("bin/java"));
            }
        }
    }
    for path in candidates {
        if let Some(major) = probe(&path).await
            && required.is_none_or(|required| major == required)
        {
            return Some(path);
        }
    }
    None
}

async fn resolve_requirement(java_path: &str, required: Option<u32>) -> Result<PathBuf, String> {
    if java_path != "auto" {
        let path = PathBuf::from(java_path);
        let actual = probe(&path).await.ok_or_else(|| {
            format!(
                "Could not run Java at {java_path}. Select automatic Java or a valid executable."
            )
        })?;
        if required.is_some_and(|major| major != actual) {
            return Err(format!(
                "This server requires Java {}, but the configured executable is Java {actual}. Select automatic Java or the required version.",
                required.unwrap()
            ));
        }
        return Ok(path);
    }
    if let Some(path) = local_java(required).await {
        return Ok(path);
    }
    let major = required.ok_or("No local Java installation was found. Install Java or set its executable path; imported JARs do not declare a Java requirement.")?;
    if (std::env::consts::OS, std::env::consts::ARCH) != ("linux", "x86_64") {
        return Err(format!(
            "Automatic Java {major} download currently supports Linux x64. Install Java {major} and set its executable path."
        ));
    }
    install_runtime(major).await
}

fn runtime_lock(major: u32) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<u32, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(major)
        .or_default()
        .clone()
}

fn official_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.adoptium.net"
                    | "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
            )
        )
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Dawg/0.1 Java runtime installer")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(240))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 6 && official_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| format!("Could not prepare the Java download: {error}"))
}

async fn metadata(client: &reqwest::Client, major: u32) -> Result<Package, String> {
    let url = format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?architecture=x64&heap_size=normal&image_type=jre&jvm_impl=hotspot&os=linux&vendor=eclipse"
    );
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("Could not fetch official Java {major} metadata: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "The official Adoptium Java {major} service returned HTTP {}. Install Java {major} locally or retry automatic setup later.",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA as u64)
    {
        return Err("The Java provider metadata exceeded its size limit.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_METADATA {
            return Err("The Java provider metadata exceeded its size limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    package(
        &serde_json::from_slice(&bytes)
            .map_err(|_| "The official Java provider returned invalid metadata.")?,
        major,
    )
}

fn package(value: &serde_json::Value, major: u32) -> Result<Package, String> {
    let releases = value
        .as_array()
        .ok_or("The Java provider returned an invalid release list.")?;
    let release = releases
        .iter()
        .find(|release| {
            release
                .pointer("/version/major")
                .or_else(|| release.pointer("/version_data/major"))
                .and_then(|n| n.as_u64())
                == Some(major as u64)
                && release
                    .pointer("/binary/architecture")
                    .and_then(|v| v.as_str())
                    == Some("x64")
                && release.pointer("/binary/os").and_then(|v| v.as_str()) == Some("linux")
                && release
                    .pointer("/binary/image_type")
                    .and_then(|v| v.as_str())
                    == Some("jre")
        })
        .ok_or_else(|| format!("No official Linux x64 Java {major} runtime is available."))?;
    let binary = release
        .pointer("/binary/package")
        .ok_or("The Java release has no package.")?;
    let url = binary
        .get("link")
        .and_then(|v| v.as_str())
        .ok_or("The Java package has no download URL.")?;
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid Java package URL.")?;
    let name = binary
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("The Java package has no name.")?;
    if !official_url(&parsed)
        || parsed.host_str() != Some("github.com")
        || !parsed.path().starts_with(&format!(
            "/adoptium/temurin{major}-binaries/releases/download/"
        ))
        || !name.starts_with(&format!("OpenJDK{major}U-jre_x64_linux_hotspot_"))
        || !name.ends_with(".tar.gz")
        || name.contains('/')
        || !parsed.path().ends_with(&format!("/{name}"))
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("The official Java release contains an unexpected download location.".into());
    }
    let checksum = binary
        .get("checksum")
        .and_then(|v| v.as_str())
        .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or(
            "The Java release has no valid SHA-256 checksum. Unverified installation was refused.",
        )?;
    let size = binary
        .get("size")
        .and_then(|v| v.as_u64())
        .filter(|size| *size > 0 && *size <= MAX_ARCHIVE)
        .ok_or("The Java package exceeds the download size limit.")?;
    Ok(Package {
        url: url.into(),
        checksum: checksum.to_ascii_lowercase(),
        size,
    })
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Could not create Java runtime storage: {error}"))?;
    if std::fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Java runtime storage must not be a symbolic link.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn verified_runtime(directory: &Path, major: u32) -> Option<PathBuf> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(directory.join(".dawg-verified.json")).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut stamp = Vec::new();
    file.take(4097).read_to_end(&mut stamp).ok()?;
    if stamp.len() > 4096 {
        return None;
    }
    let stamp: VerifiedRuntime = serde_json::from_slice(&stamp).ok()?;
    let java = directory.join("bin/java");
    let metadata = std::fs::symlink_metadata(&java).ok()?;
    if !metadata.is_file() || stamp.java_major != major || stamp.archive_sha256.len() != 64 {
        return None;
    }
    if hash_file(&java).ok()? != stamp.java_sha256 {
        return None;
    }
    Some(java)
}

async fn install_runtime(major: u32) -> Result<PathBuf, String> {
    let lock = runtime_lock(major).lock_owned().await;
    let root = dawg_core::data_dir().join("runtimes");
    private_directory(&root)?;
    let destination = root.join(format!("java-{major}"));
    if let Some(java) = verified_runtime(&destination, major) {
        return Ok(java);
    }
    let client = client()?;
    let package = metadata(&client, major).await?;
    let temporary = Temporary(root.join(format!(".java-{major}-{}", uuid::Uuid::new_v4())));
    private_directory(&temporary.0)?;
    let archive_path = temporary.0.join("runtime.tar.gz");
    let mut response = client
        .get(&package.url)
        .send()
        .await
        .map_err(|error| format!("The official Java download failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "The official Java download returned HTTP {}.",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size != package.size || size > MAX_ARCHIVE)
    {
        return Err("The Java archive size did not match the official release.".into());
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive_path)
        .await
        .map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("The Java download was interrupted: {error}"))?
    {
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or("The Java download exceeded its size limit.")?;
        if size > MAX_ARCHIVE || size > package.size {
            return Err("The Java download exceeded its size limit.".into());
        }
        digest.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    if size != package.size || format!("{:x}", digest.finalize()) != package.checksum {
        return Err(
            "Java archive SHA-256 verification failed. The unverified download was discarded."
                .into(),
        );
    }
    // Keep both inside the blocking task: cancellation cannot release the lock
    // or remove temporary files while extraction is still running.
    let result = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let temporary_guard = temporary;
        let runtime = temporary_guard.0.join("runtime");
        private_directory(&runtime)?;
        extract(&archive_path, &runtime)?;
        let java = runtime.join("bin/java");
        let stamp = VerifiedRuntime {
            java_major: major,
            archive_sha256: package.checksum,
            java_sha256: hash_file(&java)?,
        };
        std::fs::write(
            runtime.join(".dawg-verified.json"),
            serde_json::to_vec(&stamp).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let backup = root.join(format!(".previous-java-{major}-{}", uuid::Uuid::new_v4()));
        let old = destination.exists();
        if old {
            std::fs::rename(&destination, &backup).map_err(|e| e.to_string())?;
        }
        if let Err(error) = std::fs::rename(&runtime, &destination) {
            if old {
                let _ = std::fs::rename(&backup, &destination);
            }
            return Err(format!(
                "Could not activate the verified Java runtime: {error}"
            ));
        }
        if old {
            let _ = std::fs::remove_dir_all(backup);
        }
        Ok(destination.join("bin/java"))
    })
    .await
    .map_err(|error| format!("Java runtime extraction failed: {error}"))?;
    let java = result?;
    if probe(&java).await != Some(major) {
        return Err(format!(
            "The verified Java {major} runtime could not start. Install Java locally or retry setup."
        ));
    }
    Ok(java)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("Could not read the verified Java executable: {e}"))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    let mut total = 0_u64;
    loop {
        let length = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if length == 0 {
            break;
        }
        total += length as u64;
        if total > MAX_ARCHIVE {
            return Err("The Java executable exceeds its size limit.".into());
        }
        hash.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn clean_components(path: &Path) -> Result<Vec<std::ffi::OsString>, String> {
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => result.push(value.to_owned()),
            Component::CurDir => {}
            _ => return Err("The Java archive contains an unsafe path.".into()),
        }
    }
    Ok(result)
}

fn safe_parents(root: &Path, relative: &Path) -> Result<(), String> {
    let mut current = root.to_owned();
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(component) = component else {
            return Err("Unsafe Java archive parent.".into());
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err("The Java archive contains a conflicting path.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                private_directory(&current)?
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn extract(archive_path: &Path, destination: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let decoder = flate2::read::GzDecoder::new(file).take(MAX_EXPANDED + 1);
    let mut archive = tar::Archive::new(decoder);
    let mut top = None;
    let mut count = 0;
    let mut links = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("Invalid Java archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("Invalid Java archive: {e}"))?;
        count += 1;
        if count > 50_000 {
            return Err("The Java archive contains too many entries.".into());
        }
        let components = clean_components(&entry.path().map_err(|e| e.to_string())?)?;
        let Some(first) = components.first() else {
            continue;
        };
        if top.as_ref().is_some_and(|top| top != first) {
            return Err("The Java archive contains multiple roots.".into());
        }
        top = Some(first.clone());
        let relative: PathBuf = components.into_iter().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        safe_parents(destination, &relative)?;
        let path = destination.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            private_directory(&path)?;
        } else if kind.is_file() {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options
                .open(&path)
                .map_err(|e| format!("Conflicting Java archive entry: {e}"))?;
            std::io::copy(&mut entry, &mut output)
                .map_err(|e| format!("Invalid Java archive: {e}"))?;
            output.flush().map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if entry.header().mode().unwrap_or(0) & 0o111 != 0 {
                    0o700
                } else {
                    0o600
                };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| e.to_string())?;
            }
        } else if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|e| e.to_string())?
                .ok_or("Missing Java archive link target.")?
                .into_owned();
            if target.is_absolute() {
                return Err("The Java archive contains an unsafe symbolic link.".into());
            }
            let mut resolved = relative.parent().unwrap_or(Path::new("")).to_owned();
            for component in target.components() {
                match component {
                    Component::Normal(value) => resolved.push(value),
                    Component::CurDir => {}
                    Component::ParentDir if resolved.pop() => {}
                    _ => return Err("The Java archive link escapes the runtime directory.".into()),
                }
            }
            links.push((relative, target));
        } else {
            return Err("The Java archive contains an unsupported special entry.".into());
        }
    }
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())
        .map_err(|e| format!("Corrupt Java gzip archive: {e}"))?;
    if decoder.limit() == 0 {
        return Err("The Java archive exceeded its extraction size limit.".into());
    }
    for (relative, target) in &links {
        safe_parents(destination, relative)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, destination.join(relative))
            .map_err(|e| format!("Conflicting Java archive link: {e}"))?;
        #[cfg(not(unix))]
        {
            let _ = (relative, target);
            return Err("Automatic Java extraction requires Unix.".into());
        }
    }
    let root = destination.canonicalize().map_err(|e| e.to_string())?;
    for (relative, _) in links {
        if !destination
            .join(relative)
            .canonicalize()
            .map_err(|_| "The Java archive has a broken symbolic link.")?
            .starts_with(&root)
        {
            return Err("The Java archive symbolic link escapes its directory.".into());
        }
    }
    let java = std::fs::symlink_metadata(destination.join("bin/java"))
        .map_err(|_| "The verified Java archive contains no bin/java executable.")?;
    if !java.is_file() {
        return Err("The Java archive executable is not a regular file.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if java.permissions().mode() & 0o100 == 0 {
            return Err("The Java archive executable has no execution permission.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_and_profiles_are_exact() {
        assert_eq!(java_major("java version \"1.8.0_442\""), Some(8));
        assert_eq!(
            java_major("openjdk version \"21.0.6\" 2025-01-21"),
            Some(21)
        );
        assert_eq!(java_major("openjdk version \"25-ea\" 2025-09-16"), Some(25));
        assert_eq!(java_major("not a runtime"), None);
        assert_eq!(parse_profile(br#"{"java_major":25}"#).unwrap(), 25);
        assert!(parse_profile(br#"{"java_major":0}"#).is_err());
        assert!(parse_profile(br#"{"java_major":"21"}"#).is_err());
        assert!(parse_profile(&[b' '; 4097]).is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn profiles_refuse_symlinks_and_fifos_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let temporary = Temporary(
            std::env::temp_dir().join(format!("dawg-java-profile-{}", uuid::Uuid::new_v4())),
        );
        private_directory(&temporary.0).unwrap();
        let profile = temporary.0.join(".dawg-runtime.json");
        assert_eq!(read_requirement(&profile).await.unwrap(), None);
        let target = temporary.0.join("target");
        std::fs::write(&target, br#"{"java_major":21}"#).unwrap();
        std::os::unix::fs::symlink(&target, &profile).unwrap();
        assert!(read_requirement(&profile).await.is_err());
        std::fs::remove_file(&profile).unwrap();
        let fifo = std::ffi::CString::new(profile.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is NUL-terminated and remains alive for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), read_requirement(&profile))
                .await
                .unwrap()
                .is_err()
        );
    }
    #[test]
    fn official_package_requires_exact_platform_url_and_checksum() {
        let mut release = serde_json::json!([{"version":{"major":25},"binary":{"architecture":"x64","os":"linux","image_type":"jre","package":{"link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25/OpenJDK25U-jre_x64_linux_hotspot_25.tar.gz","name":"OpenJDK25U-jre_x64_linux_hotspot_25.tar.gz","size":100,"checksum":"a".repeat(64)}}}]);
        assert!(package(&release, 25).is_ok());
        assert!(package(&release, 21).is_err());
        release[0]["binary"]["package"]["checksum"] = serde_json::json!("bad");
        assert!(package(&release, 25).is_err());
        release[0]["binary"]["package"]["checksum"] = serde_json::json!("a".repeat(64));
        release[0]["binary"]["package"]["link"] =
            serde_json::json!("https://example.org/runtime.tar.gz");
        assert!(package(&release, 25).is_err());
        assert!(!official_url(
            &reqwest::Url::parse("http://github.com/file").unwrap()
        ));
    }
    #[test]
    fn corrupt_and_escaping_archives_are_refused() {
        let temporary = Temporary(
            std::env::temp_dir().join(format!("dawg-java-test-{}", uuid::Uuid::new_v4())),
        );
        private_directory(&temporary.0).unwrap();
        let archive = temporary.0.join("bad.tar.gz");
        std::fs::write(&archive, b"not gzip").unwrap();
        assert!(extract(&archive, &temporary.0.join("out")).is_err());
        assert!(clean_components(Path::new("root/../../escape")).is_err());
        assert!(clean_components(Path::new("/absolute")).is_err());
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "jdk/bin/java", &b"JRE"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        link.set_cksum();
        builder
            .append_link(&mut link, "jdk/escape", "../../outside")
            .unwrap();
        std::fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        let output = temporary.0.join("out");
        private_directory(&output).unwrap();
        assert!(extract(&archive, &output).is_err());
    }
    #[test]
    fn verified_archive_extracts_and_changed_executable_invalidates_cache() {
        let temporary = Temporary(
            std::env::temp_dir().join(format!("dawg-java-valid-{}", uuid::Uuid::new_v4())),
        );
        private_directory(&temporary.0).unwrap();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "jdk/bin/java", &b"JRE"[..])
            .unwrap();
        let archive = temporary.0.join("valid.tar.gz");
        let mut bytes = builder.into_inner().unwrap().finish().unwrap();
        std::fs::write(&archive, &bytes).unwrap();
        let output = temporary.0.join("runtime");
        private_directory(&output).unwrap();
        extract(&archive, &output).unwrap();
        let stamp = VerifiedRuntime {
            java_major: 25,
            archive_sha256: "a".repeat(64),
            java_sha256: hash_file(&output.join("bin/java")).unwrap(),
        };
        std::fs::write(
            output.join(".dawg-verified.json"),
            serde_json::to_vec(&stamp).unwrap(),
        )
        .unwrap();
        assert_eq!(verified_runtime(&output, 25), Some(output.join("bin/java")));
        assert!(verified_runtime(&output, 21).is_none());
        std::fs::write(output.join("bin/java"), b"changed").unwrap();
        assert!(verified_runtime(&output, 25).is_none());
        let last = bytes.len() - 5;
        bytes[last] ^= 0xff;
        std::fs::write(&archive, bytes).unwrap();
        let corrupt = temporary.0.join("corrupt");
        private_directory(&corrupt).unwrap();
        assert!(extract(&archive, &corrupt).is_err());
    }
    #[test]
    fn test_get_java_version_bad_path() {
        assert!(get_java_version("/invalid/path/to/java999").is_none());
    }
}
