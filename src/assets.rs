use std::path::{Path, PathBuf};

use crate::error::WmuError;
use crate::portal::{AssetKind, AssetRef};

pub struct DownloadedAsset {
    pub asset: AssetRef,
    pub path: PathBuf,
    pub size: u64,
}

pub async fn download_all(
    assets: &[AssetRef],
    output_dir: &Path,
) -> Result<Vec<DownloadedAsset>, WmuError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;

    let mut results = Vec::new();

    for asset in assets {
        let subdir = match asset.kind {
            AssetKind::JavaScript => "js",
            AssetKind::Css => "css",
            AssetKind::Image => "img",
        };

        let dir = output_dir.join(subdir);
        tokio::fs::create_dir_all(&dir).await?;

        let filename = asset
            .url
            .path_segments()
            .and_then(|s| s.last())
            .unwrap_or("unknown");
        let dest = dir.join(filename);

        match client.get(asset.url.as_str()).send().await {
            Ok(resp) => {
                let bytes = resp.bytes().await?;
                let size = bytes.len() as u64;
                tokio::fs::write(&dest, &bytes).await?;
                results.push(DownloadedAsset {
                    asset: asset.clone(),
                    path: dest,
                    size,
                });
            }
            Err(e) => {
                eprintln!("  warning: failed to download {}: {}", asset.url, e);
            }
        }
    }

    Ok(results)
}

pub async fn save_html(html: &str, path: &Path) -> Result<(), WmuError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, html).await?;
    Ok(())
}
