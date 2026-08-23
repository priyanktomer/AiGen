//! The EventPump: engine events in, WebView events out.
//!
//! # What it is actually for
//!
//! The engine already batches progress across downloads, so this is not where the 4 Hz cap
//! comes from. What is left for the pump is the traffic the engine cannot batch on its own,
//! because it does not know when a burst has finished:
//!
//! - **Status transitions arrive in clumps.** Ten queued downloads admitted the moment a slot
//!   frees, or a batch finishing together, is ten `State` events in the same instant — and ten
//!   separate IPC crossings, each triggering its own React render. The pump holds a dirty set
//!   keyed by download id and flushes it once per tick, so a clump costs one message. Keying by
//!   id also means a download that changes status twice inside one tick sends its final state
//!   once instead of both.
//! - **A slow WebView must not stall the engine.** The channel is a broadcast with a bounded
//!   buffer, so a UI that stops reading loses ticks rather than applying backpressure to a
//!   download. Progress is absolute, never incremental, so a dropped tick costs a frame and
//!   nothing else — which is exactly why `Lagged` is logged and shrugged off here.
//!
//! Notices are never batched or coalesced: each one is a distinct thing the user has to answer.

use std::{collections::HashMap, sync::Arc, time::Duration};
use swiftload_core::{
    events::{EngineEvent, StateChanged},
    manager::Manager,
};
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast::error::RecvError;

/// Matches the engine's own tick. A shorter flush would add messages without adding
/// information; a longer one would make a click feel unacknowledged.
const FLUSH: Duration = Duration::from_millis(250);

pub const PROGRESS: &str = "progress-tick";
pub const STATE: &str = "state-changed";
pub const CONNECTIONS: &str = "connections";
pub const NOTICE: &str = "notice";

pub fn spawn(app: AppHandle, manager: Arc<Manager>) {
    tauri::async_runtime::spawn(async move {
        let mut rx = manager.subscribe();
        let mut dirty: HashMap<String, StateChanged> = HashMap::new();
        let mut ticker = tokio::time::interval(FLUSH);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                received = rx.recv() => match received {
                    Ok(EngineEvent::Progress { downloads }) => {
                        let _ = app.emit(PROGRESS, downloads);
                    }
                    Ok(EngineEvent::Connections(c)) => {
                        let _ = app.emit(CONNECTIONS, c);
                    }
                    Ok(EngineEvent::Notice(n)) => {
                        let _ = app.emit(NOTICE, n);
                    }
                    // Held back and coalesced; see the module comment.
                    Ok(EngineEvent::State(s)) => {
                        dirty.insert(s.id.clone(), s);
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::debug!("UI fell behind by {n} events; state is re-read, not replayed");
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = ticker.tick() => {
                    if !dirty.is_empty() {
                        let batch: Vec<StateChanged> = dirty.drain().map(|(_, v)| v).collect();
                        let _ = app.emit(STATE, batch);
                    }
                }
            }
        }
    });
}
