//! Official server downloads. Metadata-selected artifacts are downloaded over
//! HTTPS and installed atomically only after integrity checks succeed.

use dawg_core::ServerKind;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_JAR_BYTES: u64 = 512 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const USER_AGENT: &str = "Dawg/0.1 (https://github.com/DasFletchi/Dawg)";
const TRUSTED_HOSTS: &[&str] = &[
    "piston-meta.mojang.com",
    "piston-data.mojang.com",
    "launchermeta.mojang.com",
    "launcher.mojang.com",
    "fill.papermc.io",
    "fill-data.papermc.io",
    "meta.fabricmc.net",
    "maven.minecraftforge.net",
    "files.minecraftforge.net",
];

#[derive(Debug)]
enum Checksum {
    Sha1(String),
    Sha256(String),
    // Fabric's executable server launcher endpoint does not publish a checksum.
    // HTTPS, exact host restrictions and archive validation still apply.
    FabricHttps,
}

#[derive(Debug)]
struct Artifact {
    url: String,
    checksum: Checksum,
}

/// Install the selected, exact game version. Existing JARs remain intact if
/// lookup, downloading, validation or the final atomic rename fails.
pub async fn install(
    kind: &ServerKind,
    version: &str,
    destination: &Path,
    java_path: &str,
) -> Result<(), String> {
    validate_version(version)?;
    let client = client()?;
    let required_java = super::catalog::required_java(version).await?;
    let artifact = match kind {
        ServerKind::Vanilla => vanilla(&client, version).await?,
        ServerKind::Paper => paper(&client, version).await?,
        ServerKind::Fabric => fabric(&client, version).await?,
        ServerKind::Forge => {
            super::forge::install(&client, version, destination, java_path).await?;
            save_runtime(destination, required_java).await?;
            return Ok(());
        }
    };
    download(&client, &artifact, destination).await?;
    save_runtime(destination, required_java).await
}

async fn save_runtime(destination: &Path, java_major: Option<u32>) -> Result<(), String> {
    let Some(java_major) = java_major else {
        return Ok(());
    };
    let temporary = destination.join(format!(".dawg-runtime-{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(
        &temporary,
        serde_json::to_vec(&serde_json::json!({"java_major":java_major}))
            .map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| format!("Could not save required Java version: {e}"))?;
    tokio::fs::rename(temporary, destination.join(".dawg-runtime.json"))
        .await
        .map_err(|e| format!("Could not activate Java version metadata: {e}"))
}

fn validate_version(version: &str) -> Result<(), String> {
    if version.is_empty()
        || version.len() > 100
        || version.trim() != version
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" .-_+".contains(&b))
    {
        return Err("Minecraft version must be an exact version such as 1.21.11".into());
    }
    Ok(())
}

fn validate_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|e| format!("Invalid official download URL: {e}"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || !url
            .host_str()
            .is_some_and(|host| TRUSTED_HOSTS.contains(&host))
    {
        return Err("Download metadata points outside the supported official HTTPS hosts".into());
    }
    Ok(url)
}

pub(super) fn client() -> Result<Client, String> {
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("Too many download redirects")
            } else if validate_url(attempt.url().as_str()).is_ok() {
                attempt.follow()
            } else {
                attempt.error("Refusing download redirect outside official HTTPS hosts")
            }
        }))
        .build()
        .map_err(|e| format!("Could not initialize HTTPS client: {e}"))
}

pub(super) async fn json<T: DeserializeOwned>(client: &Client, url: &str) -> Result<T, String> {
    let bytes = text(client, url).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Official server metadata was invalid: {e}"))
}

pub(super) async fn text(client: &Client, url: &str) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(validate_url(url)?)
        .timeout(Duration::from_secs(45))
        .send()
        .await
        .map_err(|e| format!("Could not fetch server metadata: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Server metadata service returned an error: {e}"))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Metadata read failed: {e}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_METADATA_BYTES {
            return Err("Server metadata exceeded the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct VanillaManifest {
    versions: Vec<VanillaVersion>,
}

#[derive(Deserialize)]
struct VanillaVersion {
    id: String,
    url: String,
}

#[derive(Deserialize)]
struct VanillaDownloads {
    downloads: HashMap<String, VanillaDownload>,
}

#[derive(Deserialize)]
struct VanillaDownload {
    url: String,
    sha1: String,
}

async fn vanilla(client: &Client, version: &str) -> Result<Artifact, String> {
    let manifest: VanillaManifest = json(
        client,
        "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
    )
    .await?;
    let selected = manifest
        .versions
        .into_iter()
        .find(|item| item.id == version)
        .ok_or_else(|| format!("Minecraft version {version} was not found in Mojang's manifest"))?;
    let metadata: VanillaDownloads = json(client, &selected.url).await?;
    let server = metadata
        .downloads
        .get("server")
        .ok_or_else(|| format!("Minecraft {version} has no official dedicated server download"))?;
    Ok(Artifact {
        url: server.url.clone(),
        checksum: Checksum::Sha1(server.sha1.clone()),
    })
}

#[derive(Deserialize)]
struct PaperBuild {
    channel: String,
    downloads: HashMap<String, PaperDownload>,
}

#[derive(Deserialize)]
struct PaperDownload {
    url: String,
    checksums: PaperChecksums,
}

#[derive(Deserialize)]
struct PaperChecksums {
    sha256: String,
}

async fn paper(client: &Client, version: &str) -> Result<Artifact, String> {
    let builds: Vec<PaperBuild> = json(
        client,
        &format!("https://fill.papermc.io/v3/projects/paper/versions/{version}/builds"),
    )
    .await?;
    let server = builds.iter()
        .filter(|build| matches!(build.channel.as_str(), "STABLE" | "RECOMMENDED"))
        .find_map(|build| build.downloads.get("server:default"))
        .or_else(|| {
            // A user explicitly selecting an upstream prerelease expects its
            // published build, even before Paper promotes it to stable.
            let prerelease = version.contains("-rc") || version.contains("-pre") || version.contains("snapshot");
            prerelease.then(|| builds.iter().find_map(|build| build.downloads.get("server:default"))).flatten()
        })
        .ok_or_else(|| format!("No stable Paper build exists for Minecraft {version}. Choose a supported version; Dawg will not substitute another version."))?;
    Ok(Artifact {
        url: server.url.clone(),
        checksum: Checksum::Sha256(server.checksums.sha256.clone()),
    })
}

#[derive(Deserialize)]
struct FabricLoader {
    loader: FabricVersion,
}

#[derive(Deserialize)]
struct FabricVersion {
    version: String,
    stable: bool,
}

async fn fabric(client: &Client, version: &str) -> Result<Artifact, String> {
    let loaders: Vec<FabricLoader> = json(
        client,
        &format!("https://meta.fabricmc.net/v2/versions/loader/{version}"),
    )
    .await?;
    let loader = loaders
        .iter()
        .find(|item| item.loader.stable)
        .ok_or_else(|| format!("No stable Fabric loader exists for Minecraft {version}"))?;
    let installers: Vec<FabricVersion> =
        json(client, "https://meta.fabricmc.net/v2/versions/installer").await?;
    let installer = installers
        .iter()
        .find(|item| item.stable)
        .ok_or_else(|| "Fabric did not publish a stable installer".to_string())?;
    validate_version(&loader.loader.version)?;
    validate_version(&installer.version)?;
    Ok(Artifact {
        url: format!(
            "https://meta.fabricmc.net/v2/versions/loader/{}/{}/{}/server/jar",
            version, loader.loader.version, installer.version
        ),
        checksum: Checksum::FabricHttps,
    })
}

async fn download(client: &Client, artifact: &Artifact, destination: &Path) -> Result<(), String> {
    download_named(client, artifact, destination, "server.jar").await
}

pub(super) async fn download_installer(
    client: &Client,
    url: String,
    sha1: String,
    destination: &Path,
) -> Result<(), String> {
    download_named(
        client,
        &Artifact {
            url,
            checksum: Checksum::Sha1(sha1),
        },
        destination,
        "installer.jar",
    )
    .await
}

async fn download_named(
    client: &Client,
    artifact: &Artifact,
    destination: &Path,
    file_name: &str,
) -> Result<(), String> {
    let mut response = client
        .get(validate_url(&artifact.url)?)
        .send()
        .await
        .map_err(|e| format!("Could not download the server JAR: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Server download failed: {e}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JAR_BYTES)
    {
        return Err("Server download exceeded the 512 MiB size limit".into());
    }
    tokio::fs::create_dir_all(destination)
        .await
        .map_err(|e| format!("Could not create server directory: {e}"))?;
    let temporary = destination.join(format!(".server-{}.part", uuid::Uuid::new_v4()));
    let result = async {
        let mut output = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|e| format!("Could not create download file: {e}"))?;
        let mut sha1 = Sha1::new();
        let mut sha256 = Sha256::new();
        let mut total = 0_u64;
        let mut prefix = Vec::with_capacity(4);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("Server download interrupted: {e}"))?
        {
            total = total.saturating_add(chunk.len() as u64);
            if total > MAX_JAR_BYTES {
                return Err("Server download exceeded the 512 MiB size limit".into());
            }
            if prefix.len() < 4 {
                prefix.extend(chunk.iter().take(4 - prefix.len()));
            }
            sha1.update(&chunk);
            sha256.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|e| format!("Could not save downloaded JAR: {e}"))?;
        }
        verify(
            &artifact.checksum,
            &format!("{:x}", sha1.finalize()),
            &format!("{:x}", sha256.finalize()),
            &prefix,
            total,
        )?;
        output
            .flush()
            .await
            .map_err(|e| format!("Could not flush JAR: {e}"))?;
        output
            .sync_all()
            .await
            .map_err(|e| format!("Could not sync JAR: {e}"))?;
        drop(output);
        // Fabric publishes no checksum for this endpoint. Parse the actual ZIP
        // structure and read its manifest (including decompression and CRC
        // checking) so a truncated response with a ZIP magic prefix cannot be
        // installed. Also check other providers after their checksum succeeds.
        validate_jar(&temporary)?;
        tokio::fs::rename(&temporary, destination.join(file_name))
            .await
            .map_err(|e| format!("Could not activate downloaded JAR: {e}"))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

pub fn is_installed(directory: &Path, kind: ServerKind) -> bool {
    launch_args(directory, kind).is_ok()
}

pub fn launch_args(directory: &Path, kind: ServerKind) -> Result<Vec<String>, String> {
    if kind == ServerKind::Forge {
        return super::forge::launch_args(directory);
    }
    if !directory.join("server.jar").is_file() {
        return Err("Install a server JAR before starting.".into());
    }
    Ok(vec!["-jar".into(), "server.jar".into()])
}

/// Validate a downloaded or imported executable JAR without extracting it.
pub fn validate_jar(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Could not open server JAR: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Could not inspect server JAR: {e}"))?;
    if !metadata.is_file() || metadata.len() > MAX_JAR_BYTES {
        return Err("Server JAR must be a regular file of at most 512 MiB".into());
    }
    validate_archive(file)
}

fn validate_archive(reader: impl Read + Seek) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| format!("Server JAR is not a complete ZIP archive: {e}"))?;
    let manifest = archive
        .by_name("META-INF/MANIFEST.MF")
        .map_err(|e| format!("Server JAR is missing its Java manifest: {e}"))?;
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    if !manifest.is_file() || manifest.size() > MAX_MANIFEST_BYTES {
        return Err("Server JAR manifest is invalid or exceeds 1 MiB".into());
    }
    let mut bytes = Vec::new();
    manifest
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Server JAR manifest failed integrity validation: {e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("Server JAR manifest exceeds 1 MiB".into());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| format!("Server JAR manifest is not valid UTF-8: {e}"))?;
    if !text
        .lines()
        .any(|line| line.starts_with("Manifest-Version:"))
        || !text.lines().any(|line| {
            line.strip_prefix("Main-Class:")
                .is_some_and(|name| !name.trim().is_empty())
        })
    {
        return Err("Server JAR must contain an executable Java manifest with Main-Class".into());
    }
    Ok(())
}

fn verify(
    checksum: &Checksum,
    actual_sha1: &str,
    actual_sha256: &str,
    prefix: &[u8],
    size: u64,
) -> Result<(), String> {
    if size < 4 || prefix != b"PK\x03\x04" {
        return Err("The download is not a JAR/ZIP archive; the previous JAR was preserved".into());
    }
    let valid = match checksum {
        Checksum::Sha1(expected) => {
            expected.len() == 40 && expected.eq_ignore_ascii_case(actual_sha1)
        }
        Checksum::Sha256(expected) => {
            expected.len() == 64 && expected.eq_ignore_ascii_case(actual_sha256)
        }
        Checksum::FabricHttps => true,
    };
    if valid {
        Ok(())
    } else {
        Err("The server JAR checksum did not match official metadata; the previous JAR was preserved".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn jar(manifest: Option<&str>) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        if let Some(manifest) = manifest {
            writer
                .start_file(
                    "META-INF/MANIFEST.MF",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(manifest.as_bytes()).unwrap();
        } else {
            writer
                .start_file(
                    "not-a-manifest.txt",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"not executable").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn jars_require_a_complete_archive_and_executable_manifest() {
        let valid = jar(Some(
            "Manifest-Version: 1.0\r\nMain-Class: example.Server\r\n\r\n",
        ));
        assert!(validate_archive(Cursor::new(&valid)).is_ok());
        assert!(validate_archive(Cursor::new(&valid[..4])).is_err());
        assert!(validate_archive(Cursor::new(&valid[..valid.len() - 22])).is_err());
        assert!(validate_archive(Cursor::new(jar(None))).is_err());
        assert!(validate_archive(Cursor::new(jar(Some("Manifest-Version: 1.0\r\n")))).is_err());
    }

    #[test]
    fn only_official_https_downloads_are_accepted() {
        assert!(validate_url("https://piston-data.mojang.com/v1/objects/abc/server.jar").is_ok());
        assert!(validate_url("http://piston-data.mojang.com/server.jar").is_err());
        assert!(validate_url("https://piston-data.mojang.com.evil.test/server.jar").is_err());
        assert!(validate_url("https://user:password@meta.fabricmc.net/server.jar").is_err());
        assert!(validate_url("https://meta.fabricmc.net:8080/server.jar").is_err());
    }

    #[test]
    fn checksums_reject_corrupt_jars_and_html_errors() {
        let bytes = b"PK\x03\x04example jar data";
        let sha1 = format!("{:x}", Sha1::digest(bytes));
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        assert!(
            verify(
                &Checksum::Sha1(sha1.clone()),
                &sha1,
                &sha256,
                &bytes[..4],
                bytes.len() as u64
            )
            .is_ok()
        );
        assert!(
            verify(
                &Checksum::Sha256(sha256.clone()),
                &sha1,
                &sha256,
                &bytes[..4],
                bytes.len() as u64
            )
            .is_ok()
        );
        assert!(
            verify(
                &Checksum::Sha256("0".repeat(64)),
                &sha1,
                &sha256,
                &bytes[..4],
                bytes.len() as u64
            )
            .is_err()
        );
        assert!(verify(&Checksum::FabricHttps, &sha1, &sha256, b"<htm", 50).is_err());
        assert!(verify(&Checksum::FabricHttps, &sha1, &sha256, b"PK", 2).is_err());
    }

    #[test]
    fn version_cannot_escape_api_path() {
        assert!(validate_version("1.21.11").is_ok());
        assert!(validate_version("26.1-snapshot-1").is_ok());
        assert!(validate_version("1.21/../../latest").is_err());
        assert!(validate_version("1.21?foo=bar").is_err());
        assert!(validate_version("").is_err());
    }
}
