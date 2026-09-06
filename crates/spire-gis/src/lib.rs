//! spire-gis — Spire-based application core (the Rust core the SwiftUI app
//! embeds as `libspire-gis.dylib`).
//!
//! Built on spire-actor (the actor runtime) and spire-core (the memory-graph /
//! spatial / vector-tile store). The SwiftUI app talks JSON over the
//! `spire_send_json` FFI entry point below; every request is routed by
//! [`coordinator::route_request`] to the actor system.

pub mod actors;
pub mod config;
pub mod coordinator;
pub mod models;

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use spire_actor::Actor;
use spire_core::actors::{MemoryGraphActor, MemoryGraphMessage, TileActor, TileMessage};
use tokio::sync::mpsc;

use crate::actors::import::{ImportActor, ImportMessage};
use crate::actors::layer::{LayerActor, LayerMessage};

// ============================================================================
// App state: one tokio runtime + the actor senders, built lazily on first FFI
// call (mirrors spire-code's `ffi.rs` composition root).
// ============================================================================

struct AppState {
    runtime: tokio::runtime::Runtime,
    graph: mpsc::Sender<MemoryGraphMessage>,
    layers: mpsc::Sender<LayerMessage>,
    import: mpsc::Sender<ImportMessage>,
    tile: mpsc::Sender<TileMessage>,
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Mutex<Option<AppState>>> = OnceLock::new();

fn state_mutex() -> &'static Mutex<Option<AppState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn lock_state() -> std::sync::MutexGuard<'static, Option<AppState>> {
    state_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Initialize the actor system + GIS store once (idempotent).
fn init() {
    if INITIALIZED.load(Ordering::Acquire) {
        return;
    }
    let data_dir = config::gis_data_dir();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (graph, layers, import, tile) = runtime.block_on(async {
        let _ = std::fs::create_dir_all(&data_dir);
        let (graph_tx, rx) = mpsc::channel::<MemoryGraphMessage>(64);
        let _join = MemoryGraphActor::new().spawn(rx);

        let (t, r) = tokio::sync::oneshot::channel();
        graph_tx
            .send(MemoryGraphMessage::Initialize {
                data_dir: data_dir.clone(),
                reply_to: t,
            })
            .await
            .expect("send store init");
        r.await
            .expect("store init reply")
            .unwrap_or_else(|e| panic!("GIS store init failed at {}: {e}", data_dir.display()));

        let (layer_tx, lrx) = mpsc::channel::<LayerMessage>(64);
        let _ljoin = LayerActor::new(graph_tx.clone()).spawn(lrx);

        let (import_tx, irx) = mpsc::channel::<ImportMessage>(64);
        let _ijoin = ImportActor::new(graph_tx.clone()).spawn(irx);

        let (tile_tx, trx) = mpsc::channel::<TileMessage>(64);
        let _tjoin = TileActor::new(graph_tx.clone()).spawn(trx);

        (graph_tx, layer_tx, import_tx, tile_tx)
    });

    let mut guard = lock_state();
    *guard = Some(AppState {
        runtime,
        graph,
        layers,
        import,
        tile,
    });
    INITIALIZED.store(true, Ordering::Release);
}

// === MORE ===

/// Process one JSON request string → JSON reply string
/// (`{"ok":true,"result":…}` or `{"ok":false,"error":…}`).
fn process_request(request: &str) -> String {
    let req: Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(e) => return reply_json(Err(format!("invalid json: {e}"))),
    };
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m,
        _ => return reply_json(Err("missing method".to_string())),
    };
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Clone the senders + a runtime handle under the lock, THEN drop the lock
    // before blocking — holding STATE across a block_on that awaits an actor
    // reply deadlocks any other RPC needing the same lock.
    let result = {
        let guard = lock_state();
        let state = match guard.as_ref() {
            Some(s) => s,
            None => return reply_json(Err("core not initialized".to_string())),
        };
        let runtime = state.runtime.handle().clone();
        let graph = state.graph.clone();
        let layers = state.layers.clone();
        let import = state.import.clone();
        let tile = state.tile.clone();
        drop(guard);
        runtime.block_on(async move {
            coordinator::route_request(&graph, &layers, &import, &tile, method, &params).await
        })
    };
    reply_json(result)
}

fn reply_json(result: Result<Value, String>) -> String {
    let envelope = match result {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    };
    envelope.to_string()
}

// ============================================================================
// FFI
// ============================================================================

/// JSON request → JSON reply (`{method, params}` in, envelope out).
#[no_mangle]
pub extern "C" fn spire_send_json(
    request: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    use std::panic;
    let outcome = panic::catch_unwind(|| {
        init();
        let request = match unsafe { CStr::from_ptr(request) }.to_str() {
            Ok(s) => s.to_string(),
            Err(e) => {
                return CString::new(format!(r#"{{"ok":false,"error":"utf8: {e}"}}"#))
                    .unwrap()
                    .into_raw()
            }
        };
        let response = process_request(&request);
        CString::new(response).unwrap().into_raw()
    });
    match outcome {
        Ok(ptr) => ptr,
        Err(info) => {
            let msg = info
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            CString::new(format!(r#"{{"ok":false,"error":"panic: {msg}"}}"#))
                .unwrap()
                .into_raw()
        }
    }
}

/// Free a string previously returned by [`spire_send_json`].
#[no_mangle]
pub extern "C" fn spire_free_string(p: *mut std::os::raw::c_char) {
    if !p.is_null() {
        unsafe { drop(CString::from_raw(p)) };
    }
}
