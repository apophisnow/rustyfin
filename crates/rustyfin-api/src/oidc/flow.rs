//! The sign-in flows (B21): one per Start, held in memory until Complete hands out the token, a
//! callback fails, or the flow's ten minutes pass. At most `FLOW_CAP` are held; the oldest goes first.

use std::collections::HashMap;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use openidconnect::CsrfToken;
use uuid::Uuid;

use super::OidcState;

/// How long a flow lives from its Start. The flow cookie lasts as long.
pub(crate) const FLOW_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// The most flows held at once.
const FLOW_CAP: usize = 1000;

/// One sign-in in progress: what the callback must match, and the user a good callback signed in.
pub(crate) struct Flow {
    /// The state sent to the provider. The callback must return it.
    pub(crate) state: String,
    /// The nonce sent to the provider. The ID token must carry it.
    pub(crate) nonce: String,
    /// The PKCE verifier for the token request.
    pub(crate) verifier: String,
    pub(crate) created: Instant,
    /// The user a good callback signed in. Complete takes the flow and this user.
    pub(crate) signed_in: Option<Uuid>,
}

impl Flow {
    fn is_live(&self) -> bool {
        self.created.elapsed() < FLOW_LIFETIME
    }
}

/// The flows held in memory, by id.
#[derive(Default)]
pub(crate) struct FlowStore {
    flows: HashMap<String, Flow>,
}

impl FlowStore {
    /// Holds a flow under `id`. Expired flows go first, then the oldest while the store is full.
    fn insert(&mut self, id: String, flow: Flow) {
        self.flows.retain(|_, held| held.is_live());
        while self.flows.len() >= FLOW_CAP {
            let oldest = self
                .flows
                .iter()
                .min_by_key(|(_, held)| held.created)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(oldest) => {
                    self.flows.remove(&oldest);
                }
                None => break,
            }
        }
        self.flows.insert(id, flow);
    }
}

impl OidcState {
    fn store(&self) -> std::sync::MutexGuard<'_, FlowStore> {
        self.flows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a flow and returns its id, the value of the flow cookie.
    pub(crate) fn insert_flow(&self, flow: Flow) -> String {
        let id = CsrfToken::new_random().secret().clone();
        self.store().insert(id.clone(), flow);
        id
    }

    /// Takes a flow out of the store. A callback holds the flow while it works, and puts it back
    /// only when it has signed the user in. None for an unknown or expired flow.
    pub(crate) fn take_flow(&self, id: &str) -> Option<Flow> {
        let flow = self.store().flows.remove(id)?;
        flow.is_live().then_some(flow)
    }

    /// Puts back a flow that a good callback has signed in.
    pub(crate) fn put_flow(&self, id: String, flow: Flow) {
        self.store().insert(id, flow);
    }

    /// Completes a flow: the user it signed in, if it has signed one in. A completed flow is gone.
    pub(crate) fn finish_flow(&self, id: &str) -> Option<Uuid> {
        let mut store = self.store();
        let flow = store.flows.get(id)?;
        let user = flow.signed_in.filter(|_| flow.is_live())?;
        store.flows.remove(id);
        Some(user)
    }
}
