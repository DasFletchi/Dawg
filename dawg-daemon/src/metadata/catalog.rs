//! Exact upstream versions, including Mojang's year-based IDs and snapshots.
//! A loader catalog never substitutes a different Minecraft release.

use super::installer::{client, json};
use dawg_core::{MinecraftVersion, ServerKind, VersionCatalog, VersionChannel};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const CACHE_TTL: Duration = Duration::from_secs(300);
type Cache = Vec<(ServerKind, Instant, VersionCatalog)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
static FETCH_LOCKS: OnceLock<Vec<Mutex<()>>> = OnceLock::new();

#[derive(Deserialize)]
struct Manifest {
    latest: Latest,
    versions: Vec<ManifestVersion>,
}

#[derive(Deserialize)]
struct Latest {
    release: String,
    snapshot: String,
}

#[derive(Deserialize)]
struct ManifestVersion {
    id: String,
    #[serde(rename = "type")]
    channel: String,
    url: String,
}

#[derive(Deserialize)]
struct FabricGame {
    version: String,
    stable: bool,
}

#[derive(Deserialize)]
struct PaperProject {
    versions: HashMap<String, Vec<String>>,
}

pub async fn fetch(kind: ServerKind) -> Result<VersionCatalog, String> {
    // Coalesce identical requests while allowing another loader to load when
    // one upstream service is slow or unavailable.
    let index = ServerKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("all server kinds");
    let _fetch = FETCH_LOCKS
        .get_or_init(|| ServerKind::ALL.iter().map(|_| Mutex::new(())).collect())[index]
        .lock()
        .await;
    let cached = CACHE.get_or_init(|| Mutex::new(Vec::new())).lock().await;
    if let Some((_, _, catalog)) = cached
        .iter()
        .find(|(cached_kind, checked, _)| *cached_kind == kind && checked.elapsed() < CACHE_TTL)
    {
        return Ok(catalog.clone());
    }
    drop(cached);
    let client = client()?;
    let manifest: Manifest = json(&client, MANIFEST).await?;
    let versions = match kind {
        ServerKind::Vanilla => manifest.versions.iter().map(manifest_version).collect(),
        ServerKind::Fabric => {
            let games: Vec<FabricGame> =
                json(&client, "https://meta.fabricmc.net/v2/versions/game").await?;
            // Fabric also lists historical experimental client-only variants
            // outside Mojang's server manifest. The installer needs an actual
            // official game metadata entry and Java requirement for each choice.
            let official: HashSet<_> = manifest
                .versions
                .iter()
                .map(|version| version.id.as_str())
                .collect();
            games
                .into_iter()
                .filter(|game| official.contains(game.version.as_str()))
                .map(|game| MinecraftVersion {
                    id: game.version,
                    channel: if game.stable {
                        VersionChannel::Release
                    } else {
                        VersionChannel::Snapshot
                    },
                })
                .collect()
        }
        ServerKind::Paper => {
            let project: PaperProject =
                json(&client, "https://fill.papermc.io/v3/projects/paper").await?;
            let supported: HashSet<_> = project.versions.into_values().flatten().collect();
            manifest
                .versions
                .iter()
                .filter(|version| supported.contains(&version.id))
                .map(manifest_version)
                .collect()
        }
        ServerKind::Forge => {
            let available = super::forge::versions(&client).await?;
            let supported: HashSet<_> = available
                .iter()
                .filter_map(|build| super::forge::game_version(build))
                .collect();
            manifest
                .versions
                .iter()
                .filter(|version| supported.contains(version.id.as_str()))
                .map(manifest_version)
                .collect()
        }
    };
    let catalog = finish_catalog(kind, versions, &manifest.latest);
    if catalog.versions.is_empty() {
        return Err(format!(
            "{kind} did not publish any supported Minecraft versions."
        ));
    }
    let mut cache = CACHE.get().expect("initialized cache").lock().await;
    cache.retain(|(cached_kind, _, _)| *cached_kind != kind);
    cache.push((kind, Instant::now(), catalog.clone()));
    Ok(catalog)
}

fn manifest_version(version: &ManifestVersion) -> MinecraftVersion {
    MinecraftVersion {
        id: version.id.clone(),
        channel: match version.channel.as_str() {
            "release" => VersionChannel::Release,
            "snapshot" => VersionChannel::Snapshot,
            _ => VersionChannel::Other,
        },
    }
}

fn finish_catalog(
    kind: ServerKind,
    mut versions: Vec<MinecraftVersion>,
    latest: &Latest,
) -> VersionCatalog {
    let mut seen = HashSet::new();
    versions.retain(|version| seen.insert(version.id.clone()));
    let choose = |id: &str, channel| {
        versions
            .iter()
            .find(|version| version.id == id && version.channel == channel)
            .or_else(|| versions.iter().find(|version| version.channel == channel))
            .map(|version| version.id.clone())
    };
    VersionCatalog {
        kind,
        latest_release: choose(&latest.release, VersionChannel::Release),
        latest_snapshot: choose(&latest.snapshot, VersionChannel::Snapshot),
        versions,
    }
}

/// Read the exact upstream Java requirement; never infer it from a version
/// prefix (26.x and older weekly snapshots use different naming schemes).
pub async fn required_java(version: &str) -> Result<Option<u32>, String> {
    #[derive(Deserialize)]
    struct VersionMetadata {
        #[serde(rename = "javaVersion")]
        java_version: Option<JavaVersion>,
    }
    #[derive(Deserialize)]
    struct JavaVersion {
        #[serde(rename = "majorVersion")]
        major: u32,
    }
    let client = client()?;
    let manifest: Manifest = json(&client, MANIFEST).await?;
    let selected = manifest
        .versions
        .iter()
        .find(|item| item.id == version)
        .ok_or_else(|| format!("Minecraft {version} is not in the official version manifest."))?;
    let metadata: VersionMetadata = json(&client, &selected.url).await?;
    Ok(metadata.java_version.map(|version| version.major))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_versions_and_snapshots_stay_exact_and_loader_specific() {
        let latest = Latest {
            release: "26.3".into(),
            snapshot: "26.4-snapshot-5".into(),
        };
        let catalog = finish_catalog(
            ServerKind::Fabric,
            vec![
                MinecraftVersion {
                    id: "26.4-snapshot-5".into(),
                    channel: VersionChannel::Snapshot,
                },
                MinecraftVersion {
                    id: "26.3".into(),
                    channel: VersionChannel::Release,
                },
                MinecraftVersion {
                    id: "26.3".into(),
                    channel: VersionChannel::Release,
                },
                MinecraftVersion {
                    id: "1.21.11".into(),
                    channel: VersionChannel::Release,
                },
            ],
            &latest,
        );
        assert_eq!(catalog.latest_release.as_deref(), Some("26.3"));
        assert_eq!(catalog.latest_snapshot.as_deref(), Some("26.4-snapshot-5"));
        assert_eq!(catalog.versions.len(), 3);
        let older_loader = finish_catalog(
            ServerKind::Paper,
            vec![MinecraftVersion {
                id: "1.21.11".into(),
                channel: VersionChannel::Release,
            }],
            &latest,
        );
        assert_eq!(older_loader.latest_release.as_deref(), Some("1.21.11"));
        assert!(older_loader.latest_snapshot.is_none());
    }

    #[test]
    fn channels_come_from_upstream_not_version_string_heuristics() {
        let release = ManifestVersion {
            id: "26.3".into(),
            channel: "release".into(),
            url: String::new(),
        };
        let snapshot = ManifestVersion {
            id: "26w14a".into(),
            channel: "snapshot".into(),
            url: String::new(),
        };
        assert_eq!(manifest_version(&release).channel, VersionChannel::Release);
        assert_eq!(
            manifest_version(&snapshot).channel,
            VersionChannel::Snapshot
        );
    }

    #[tokio::test]
    #[ignore = "requires official metadata services"]
    async fn live_official_catalogs_and_java_requirements() {
        for kind in [ServerKind::Vanilla, ServerKind::Paper, ServerKind::Fabric] {
            let catalog = fetch(kind).await.unwrap();
            assert!(catalog.versions.iter().any(|version| version.id == "26.3"));
            assert!(catalog.latest_release.is_some());
        }
        assert_eq!(required_java("26.3").await.unwrap(), Some(25));
    }
}
