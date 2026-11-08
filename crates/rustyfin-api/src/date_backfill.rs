//! The startup task for the date added backfill: it runs the pending backfill, then drops the cached
//! answers that hold `DateCreated`, so the new dates show at once.

use crate::state::AppState;

/// Runs the pending backfill, if any, then drops state.cache.item_details and
/// state.cache.search_results (also after an error, which it logs as a warning).
pub async fn run(state: &AppState) {
    match rustyfin_core::date_backfill::run_pending(&state.db).await {
        Ok(None) => return,
        Ok(Some(_)) => {}
        Err(e) => tracing::warn!(
            "date added backfill stopped: {e}; the next start goes on from the last batch it saved"
        ),
    }
    state.cache.item_details.invalidate_all();
    state.cache.search_results.invalidate_all();
}
