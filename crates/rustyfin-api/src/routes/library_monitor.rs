use std::future::Future;
use std::pin::Pin;

use rustyfin_core::library_monitor::RescanRunner;
use rustyfin_core::scan_progress::ScanTicket;
use rustyfin_core::scanner::{self, ScanChanges, ScanScope};
use rustyfin_core::tmdb_client::TmdbClient;
use rustyfin_model::library::Library;
use rustyfin_model::server_info::ServerConfiguration;
use rustyfin_model::ws_event::WsEvent;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

use crate::cache::{ApiCache, ImageCache};
use crate::state::AppState;

/// The library monitor's runner in the API. A rescan scans as a library scan does, without progress
/// events. When it changed something, the caches are cleared and `LibraryChanged` is sent.
#[derive(Clone)]
pub struct LibraryMonitorRunner {
    db: SqlitePool,
    cache: ApiCache,
    image_cache: ImageCache,
    ws_broadcast: broadcast::Sender<WsEvent>,
    config: Arc<RwLock<ServerConfiguration>>,
    http_client: reqwest::Client,
    tmdb: TmdbClient,
}

impl LibraryMonitorRunner {
    /// The runner for the state `state`, which holds the same handles as the runner's fields.
    pub fn for_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            cache: state.cache.clone(),
            image_cache: state.image_cache.clone(),
            ws_broadcast: state.ws_broadcast.clone(),
            config: state.config.clone(),
            http_client: state.http_client.clone(),
            tmdb: state.tmdb.clone(),
        }
    }
}

impl RescanRunner for LibraryMonitorRunner {
    fn rescan(
        &self,
        library: Library,
        scope: ScanScope,
    ) -> Pin<Box<dyn Future<Output = Result<ScanChanges, String>> + Send>> {
        let runner = self.clone();
        Box::pin(async move {
            // The metadata providers are the ones a library scan uses.
            let config = runner.config.read().await;
            let tmdb = runner.tmdb.api(config.tmdb_api_key.as_deref());
            let omdb_key = config.omdb_api_key.clone();
            let fanart_key = config.fanart_api_key.clone();
            drop(config);

            let changes = scanner::scan_with_metadata(
                &runner.db,
                &library,
                &scope,
                &runner.http_client,
                tmdb.as_ref(),
                omdb_key.as_deref(),
                fanart_key.as_deref(),
                &ScanTicket::untracked(),
            )
            .await
            .map_err(|e| e.to_string())?;
            if changes.changed() {
                runner.cache.invalidate_on_scan();
                runner.image_cache.invalidate_all();
                let _ = runner.ws_broadcast.send(WsEvent::LibraryChanged {
                    data: json!({
                        "LibraryId": library.id.to_string(),
                        "LibraryName": library.name,
                    }),
                });
            }
            Ok(changes)
        })
    }
}
