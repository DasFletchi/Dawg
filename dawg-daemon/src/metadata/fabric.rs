use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct FabricMeta {
    pub game: Vec<GameVersion>,
    pub loader: Vec<LoaderVersion>,
    pub installer: Vec<InstallerVersion>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GameVersion {
    pub version: String,
    pub stable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoaderVersion {
    pub version: String,
    pub stable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallerVersion {
    pub url: String,
    pub maven: String,
    pub version: String,
    pub stable: bool,
}

pub async fn get_fabric_meta() -> Result<FabricMeta, reqwest::Error> {
    let url = "https://meta.fabricmc.net/v2/versions";
    let meta: FabricMeta = reqwest::get(url).await?.json().await?;
    Ok(meta)
}
