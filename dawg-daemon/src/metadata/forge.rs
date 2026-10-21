//! Modern Forge uses the official installer and Java argument files, not a
//! renamed installer JAR pretending to be a runnable Minecraft server.

use super::installer::{download_installer, text};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
const PROFILE: &str = ".dawg-launch.json";

pub(super) async fn versions(client: &Client) -> Result<Vec<String>, String> {
    let bytes = text(client, &format!("{MAVEN}/maven-metadata.xml")).await?;
    parse_versions(std::str::from_utf8(&bytes).map_err(|_| "Forge Maven metadata was not UTF-8.")?)
}

fn parse_versions(xml: &str) -> Result<Vec<String>, String> {
    // Maven's version identifiers are plain ASCII; reject entities and DTDs.
    // Only the explicit <versions> list is used, not untrusted XML paths or
    // entity expansion. Downloads are constructed from validated identifiers.
    if xml.contains("<!DOCTYPE") || xml.contains("<!ENTITY") {
        return Err("Forge Maven metadata contains unsupported XML declarations.".into());
    }
    let list = xml
        .split_once("<versions>")
        .and_then(|(_, tail)| tail.split_once("</versions>").map(|(list, _)| list))
        .ok_or("Forge Maven metadata has no version list.")?;
    let versions: Vec<_> = list
        .split("<version>")
        .skip(1)
        .filter_map(|entry| {
            entry
                .split_once("</version>")
                .map(|(version, _)| version.trim())
        })
        .filter(|version| game_version(version).is_some())
        .map(str::to_owned)
        .collect();
    if versions.is_empty() {
        return Err("Forge did not publish any supported modern server installers.".into());
    }
    Ok(versions)
}

/// Only modern Forge releases with a dedicated Unix Java argument file are
/// offered. Older Forge needs a different launch profile and older Java.
pub(super) fn game_version(build: &str) -> Option<&str> {
    if build.len() > 160
        || !build
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b".-".contains(&byte))
    {
        return None;
    }
    let (game, forge) = build.split_once('-')?;
    if forge.is_empty()
        || forge
            .split('.')
            .any(|segment| segment.is_empty() || segment.parse::<u32>().is_err())
    {
        return None;
    }
    let mut parts = game.split('.').map(str::parse::<u32>);
    let major = parts.next()?.ok()?;
    let minor = parts.next()?.ok()?;
    if parts.any(|part| part.is_err()) {
        return None;
    }
    ((major == 1 && minor >= 17) || major >= 26).then_some(game)
}

fn choose_build<'a>(versions: &'a [String], version: &str) -> Result<&'a str, String> {
    versions.iter().filter(|build| game_version(build) == Some(version))
        .max_by_key(|build| build.split_once('-').map(|(_, forge)| forge.split('.').filter_map(|part| part.parse::<u32>().ok()).collect::<Vec<_>>()).unwrap_or_default())
        .map(String::as_str).ok_or_else(|| format!("Forge has no supported server installer for Minecraft {version}. No other game version will be substituted."))
}

#[derive(Serialize, Deserialize)]
struct LaunchProfile {
    format: u32,
    forge_version: String,
    args_file: String,
}

fn argument_file(build: &str) -> String {
    format!("libraries/net/minecraftforge/forge/{build}/unix_args.txt")
}

pub(super) fn launch_args(directory: &Path) -> Result<Vec<String>, String> {
    let metadata = std::fs::metadata(directory.join(PROFILE))
        .map_err(|_| "Install the Forge server before starting it.")?;
    if metadata.len() > 4096 {
        return Err("Forge launch profile exceeds the size limit.".into());
    }
    let profile: LaunchProfile = serde_json::from_slice(
        &std::fs::read(directory.join(PROFILE))
            .map_err(|_| "Could not read the Forge launch profile.")?,
    )
    .map_err(|_| "The Forge launch profile is invalid. Reinstall this server.")?;
    if profile.format != 1
        || game_version(&profile.forge_version).is_none()
        || profile.args_file != argument_file(&profile.forge_version)
    {
        return Err("The Forge launch profile does not match an official installer layout.".into());
    }
    validate_argfile(directory, &profile.args_file)?;
    Ok(vec![format!("@{}", profile.args_file)])
}

fn validate_argfile(directory: &Path, relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("Forge arguments must remain inside this server directory.".into());
    }
    let base = directory
        .canonicalize()
        .map_err(|_| "Could not inspect the Forge server directory.")?;
    let target = directory
        .join(path)
        .canonicalize()
        .map_err(|_| "The Forge installer did not produce its required Java argument file.")?;
    if !target.starts_with(&base) || !target.is_file() {
        return Err("Forge Java arguments point outside this server directory.".into());
    }
    let size = std::fs::metadata(&target)
        .map_err(|_| "Could not inspect Forge Java arguments.")?
        .len();
    if size == 0 || size > 1024 * 1024 {
        return Err("Forge Java argument file is empty or too large.".into());
    }
    Ok(())
}

pub(super) async fn install(
    client: &Client,
    version: &str,
    directory: &Path,
    java_path: &str,
) -> Result<(), String> {
    if java_path.is_empty() || java_path == "auto" {
        return Err("Resolve the required Java runtime before running the Forge installer.".into());
    }
    let available = versions(client).await?;
    let build = choose_build(&available, version)?;
    let url = format!("{MAVEN}/{build}/forge-{build}-installer.jar");
    let digest = text(client, &format!("{url}.sha1")).await?;
    let digest = std::str::from_utf8(&digest)
        .map_err(|_| "Forge published an invalid installer checksum.")?
        .trim();
    if digest.len() != 40 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Forge did not publish a valid SHA-1 installer checksum. Unverified execution was refused.".into());
    }
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|e| format!("Could not create the server directory: {e}"))?;
    let staging = directory.join(format!(".forge-install-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&staging)
        .await
        .map_err(|e| format!("Could not prepare Forge installation: {e}"))?;
    let result = async {
        download_installer(client, url, digest.to_string(), &staging).await?;
        let mut command = Command::new(java_path);
        command.arg("-jar").arg("installer.jar").arg("--installServer")
            .current_dir(&staging).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            let parent = unsafe { libc::getpid() };
            unsafe { command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
                if libc::getppid() != parent { return Err(std::io::Error::other("Daemon exited while launching Forge installer")); }
                Ok(())
            }); }
        }
        let mut child = command.spawn().map_err(|e| format!("Could not launch the Forge installer with the selected Java: {e}"))?;
        // Drain output to avoid pipe deadlocks without accumulating arbitrary
        // installer logs or forwarding signed download URLs into API errors.
        let stdout = child.stdout.take().ok_or("Forge installer stdout is unavailable.")?;
        let stderr = child.stderr.take().ok_or("Forge installer stderr is unavailable.")?;
        let drain = |mut pipe: Box<dyn tokio::io::AsyncRead + Unpin + Send>| tokio::spawn(async move {
            let mut bytes = [0; 8192];
            loop { match pipe.read(&mut bytes).await { Ok(0) | Err(_) => return, Ok(_) => {} } }
        });
        let stdout = drain(Box::new(stdout));
        let stderr = drain(Box::new(stderr));
        let result = tokio::time::timeout(Duration::from_secs(600), child.wait()).await;
        if result.is_err() { let _ = child.kill().await; }
        stdout.abort(); stderr.abort();
        let success = result.map_err(|_| "Forge installation timed out after ten minutes; no new launch profile was activated.")?
            .map_err(|e| format!("Could not wait for the Forge installer: {e}"))?;
        if !success.success() { return Err("The official Forge installer failed. Check the selected Java version, available disk space and access to Mojang/Forge Maven downloads.".into()); }
        let args_file = argument_file(build);
        validate_argfile(&staging, &args_file)?;
        // Only installer-created libraries enter the server root; run.sh,
        // run.bat, world data and JVM settings are never executed or replaced.
        copy_libraries(&staging.join("libraries"), &directory.join("libraries")).await?;
        validate_argfile(directory, &args_file)?;
        let profile = LaunchProfile { format: 1, forge_version: build.into(), args_file };
        let temporary = directory.join(format!(".dawg-launch-{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary, serde_json::to_vec(&profile).map_err(|e| e.to_string())?).await
            .map_err(|e| format!("Could not save the Forge launch profile: {e}"))?;
        tokio::fs::rename(temporary, directory.join(PROFILE)).await.map_err(|e| format!("Could not activate the Forge launch profile: {e}"))
    }.await;
    let _ = tokio::fs::remove_dir_all(&staging).await;
    result
}

async fn copy_libraries(source: &Path, destination: &Path) -> Result<(), String> {
    let mut queue: Vec<(PathBuf, PathBuf)> = vec![(source.into(), destination.into())];
    let mut count = 0_u64;
    let mut total = 0_u64;
    while let Some((source, destination)) = queue.pop() {
        if let Ok(meta) = tokio::fs::symlink_metadata(&destination).await
            && (!meta.is_dir() || meta.file_type().is_symlink())
        {
            return Err("Forge library destination must be a real server directory.".into());
        }
        tokio::fs::create_dir_all(&destination)
            .await
            .map_err(|e| format!("Could not create Forge library directory: {e}"))?;
        let mut entries = tokio::fs::read_dir(&source)
            .await
            .map_err(|e| format!("Could not inspect installed Forge libraries: {e}"))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("Could not read installed Forge libraries: {e}"))?
        {
            count += 1;
            if count > 100_000 {
                return Err("Forge installation contains too many library files.".into());
            }
            let kind = entry
                .file_type()
                .await
                .map_err(|e| format!("Could not inspect Forge library entry: {e}"))?;
            let target = destination.join(entry.file_name());
            if kind.is_dir() {
                queue.push((entry.path(), target));
            } else if kind.is_file() {
                let metadata = entry
                    .metadata()
                    .await
                    .map_err(|e| format!("Could not inspect Forge library: {e}"))?;
                total = total.saturating_add(metadata.len());
                if total > 4 * 1024 * 1024 * 1024 {
                    return Err("Forge libraries exceed the 4 GiB installation limit.".into());
                }
                if let Ok(existing) = tokio::fs::symlink_metadata(&target).await
                    && (!existing.is_file() || existing.file_type().is_symlink())
                {
                    return Err("Forge library destination must be a regular file.".into());
                }
                tokio::fs::copy(entry.path(), target)
                    .await
                    .map_err(|e| format!("Could not install Forge library: {e}"))?;
            } else {
                return Err("Forge installer created an unsupported library entry.".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_catalog_lists_only_real_modern_installers_and_compares_build_numbers() {
        let versions = parse_versions("<metadata><versioning><versions><version>1.12.2-14.23.5.2860</version><version>1.21.1-52.0.9</version><version>1.21.1-52.0.10</version><version>1.20.1-47.4.0</version><version>../../evil</version></versions></versioning></metadata>").unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(choose_build(&versions, "1.21.1").unwrap(), "1.21.1-52.0.10");
        assert!(choose_build(&versions, "26.3").is_err());
        assert!(parse_versions("<!DOCTYPE metadata [<!ENTITY evil SYSTEM 'file:///etc/passwd'>]><versions><version>&evil;</version></versions>").is_err());
    }

    #[test]
    fn launch_profile_rejects_traversal_missing_libraries_and_symlink_escape() {
        let directory =
            std::env::temp_dir().join(format!("dawg-forge-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let build = "1.21.1-52.0.10";
        let relative = argument_file(build);
        let args = directory.join(&relative);
        std::fs::create_dir_all(args.parent().unwrap()).unwrap();
        std::fs::write(&args, "-cp libraries/example.jar\nexample.Server\n").unwrap();
        let mut profile = LaunchProfile {
            format: 1,
            forge_version: build.into(),
            args_file: relative.clone(),
        };
        let save = |profile: &LaunchProfile| {
            std::fs::write(
                directory.join(PROFILE),
                serde_json::to_vec(profile).unwrap(),
            )
            .unwrap()
        };
        save(&profile);
        assert_eq!(
            launch_args(&directory).unwrap(),
            vec![format!("@{relative}")]
        );
        profile.args_file = "../outside-args.txt".into();
        save(&profile);
        assert!(launch_args(&directory).is_err());
        profile.args_file = relative;
        save(&profile);
        std::fs::remove_file(&args).unwrap();
        assert!(launch_args(&directory).is_err());
        #[cfg(unix)]
        {
            let outside = directory.with_extension("outside");
            std::fs::write(&outside, "malicious args").unwrap();
            std::os::unix::fs::symlink(&outside, &args).unwrap();
            assert!(launch_args(&directory).is_err());
            std::fs::remove_file(outside).unwrap();
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
