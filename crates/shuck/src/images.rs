use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub expected_sha256: String,
    pub dest: PathBuf,
}

pub fn parse_manifest(contents: &str) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?;
            let name = parts.next()?;
            Some((name.to_string(), sha.to_string()))
        })
        .collect()
}

pub async fn fetch_and_verify(spec: DownloadSpec) -> Result<()> {
    let client = reqwest::Client::builder()
        .build()
        .context("building http client")?;

    let mut resp = client
        .get(&spec.url)
        .send()
        .await
        .with_context(|| format!("GET {}", spec.url))?
        .error_for_status()
        .with_context(|| format!("GET {}", spec.url))?;

    if let Some(parent) = spec.dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }

    let tmp = spec.dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("opening {}", tmp.display()))?;

    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.context("reading response chunk")? {
        hasher.update(&chunk);
        file.write_all(&chunk).await.context("writing chunk")?;
    }
    file.flush().await.context("flushing download")?;
    drop(file);

    let got = hex::encode(hasher.finalize());
    if got != spec.expected_sha256 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow!(
            "sha256 mismatch for {}: expected {}, got {}",
            spec.dest.display(),
            spec.expected_sha256,
            got,
        ));
    }

    tokio::fs::rename(&tmp, &spec.dest)
        .await
        .with_context(|| format!("renaming {} -> {}", tmp.display(), spec.dest.display()))?;
    Ok(())
}

pub async fn fetch_manifest(base_url: &str) -> Result<HashMap<String, String>> {
    let url = format!("{}/SHA256SUMS", base_url.trim_end_matches('/'));
    let body = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .text()
        .await
        .with_context(|| format!("reading body {url}"))?;
    Ok(parse_manifest(&body))
}
