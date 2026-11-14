//! Prepares the grid's pictures in the background, so that scrolling a library never waits on a
//! resize: after each scan or metadata refresh (and once at startup), every item's Primary image
//! gets its grid master (`image_proxy::GRID_MASTER`, the copy smaller grid copies are made from)
//! and its BlurHash, where it has none yet. Remote originals are kept in the data dir as the
//! image route does on first use.
//!
//! It works one picture at a time and waits while clients' renders take every slot, so it never
//! delays a page. A pass writes at most `WARM_PASS_MAX_BYTES` of masters, half the disk cache's
//! cap, so it never evicts what clients use; a library larger than that warms its top part.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustyfin_model::image::ItemImage;
use sqlx::SqlitePool;

use crate::image_proxy;

/// The most bytes of grid masters one pass writes.
pub const WARM_PASS_MAX_BYTES: u64 = image_proxy::IMAGE_CACHE_MAX_BYTES / 2;

/// How long a pass waits after a kick, so that a burst of kicks makes one pass.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

static KICKS: OnceLock<tokio::sync::Notify> = OnceLock::new();

/// What a pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WarmReport {
    /// Grid masters written.
    pub prepared: u32,
    /// BlurHashes stored.
    pub hashed: u32,
    /// Pictures that had both already.
    pub ready: u32,
    /// Pictures that could not be read or decoded.
    pub failed: u32,
    /// Bytes of masters written.
    pub bytes: u64,
    /// Whether the pass stopped at `WARM_PASS_MAX_BYTES`.
    pub capped: bool,
}

/// Starts the warmer, which runs a pass now and after every `kick`. Later calls do nothing.
pub fn start(pool: SqlitePool, data_dir: PathBuf, http_client: reqwest::Client) {
    if KICKS.set(tokio::sync::Notify::new()).is_err() {
        return;
    }
    kick();
    tokio::spawn(async move {
        let kicks = KICKS.get().expect("set above");
        loop {
            kicks.notified().await;
            tokio::time::sleep(SETTLE).await;
            let started = std::time::Instant::now();
            match warm_pass(&pool, &data_dir, &http_client).await {
                Ok(r) if r.prepared > 0 || r.hashed > 0 || r.failed > 0 => tracing::info!(
                    "prepared grid images: {} copies ({} KiB), {} BlurHashes, {} already ready, \
                     {} failed{} in {:.1} s",
                    r.prepared,
                    r.bytes / 1024,
                    r.hashed,
                    r.ready,
                    r.failed,
                    if r.capped {
                        ", stopped at the size cap,"
                    } else {
                        ""
                    },
                    started.elapsed().as_secs_f64()
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("failed to prepare grid images: {}", e),
            }
        }
    });
}

/// Asks the warmer for a pass, after the one running if any. Does nothing before `start`.
pub fn kick() {
    if let Some(kicks) = KICKS.get() {
        kicks.notify_one();
    }
}

/// One pass over every item's Primary image, the top of each library first.
pub async fn warm_pass(
    pool: &SqlitePool,
    data_dir: &Path,
    http_client: &reqwest::Client,
) -> Result<WarmReport, sqlx::Error> {
    let images = rustyfin_db::repo_image::list_primary_images(pool).await?;
    let cache_dir = data_dir.join("cache").join("images");
    let store = data_dir.join("metadata").join("remote-images");
    let mut report = WarmReport::default();
    for img in images {
        if report.bytes >= WARM_PASS_MAX_BYTES {
            report.capped = true;
            break;
        }
        while image_proxy::renders_busy() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        match warm_one(pool, &img, &cache_dir, &store, http_client).await {
            Ok((written, hashed)) => {
                if written > 0 {
                    report.prepared += 1;
                    report.bytes += written;
                }
                if hashed {
                    report.hashed += 1;
                }
                if written == 0 && !hashed {
                    report.ready += 1;
                }
            }
            Err(e) => {
                report.failed += 1;
                tracing::debug!(path = %img.path, "could not prepare grid image: {}", e);
            }
        }
    }
    Ok(report)
}

/// Prepares one picture: its grid master if missing, and its BlurHash if it has none. Returns the
/// bytes of master written (0 when it was there) and whether a BlurHash was stored.
async fn warm_one(
    pool: &SqlitePool,
    img: &ItemImage,
    cache_dir: &Path,
    store: &Path,
    http_client: &reqwest::Client,
) -> Result<(u64, bool), String> {
    let source = if img.path.starts_with("http://") || img.path.starts_with("https://") {
        image_proxy::localize_remote_image_checked(&img.path, store, Some(http_client))
            .await
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned()
    } else {
        img.path.clone()
    };
    let master_path = cache_dir.join(image_proxy::grid_master_name(&source, img.stamp));
    let existed = tokio::fs::metadata(&master_path)
        .await
        .is_ok_and(|m| m.len() > 0);
    if existed && img.blurhash.is_some() {
        return Ok((0, false));
    }
    let master = image_proxy::render_image(
        &source,
        img.stamp,
        image_proxy::Rendition::Copy {
            width: image_proxy::GRID_MASTER.0,
            height: image_proxy::GRID_MASTER.1,
        },
        cache_dir,
        Some(http_client),
    )
    .await?;
    let written = if existed { 0 } else { master.len() as u64 };
    let mut hashed = false;
    if img.blurhash.is_none() {
        let hash = tokio::task::spawn_blocking(move || image_proxy::blurhash_of(&master))
            .await
            .map_err(|e| e.to_string())?
            .ok_or("could not decode the grid master")?;
        hashed = rustyfin_db::repo_image::set_image_blurhash(pool, img, &hash)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok((written, hashed))
}
