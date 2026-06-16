// SPDX-License-Identifier: Apache-2.0
//! The hand-built `hellohq:plugin/storage` + `hellohq:plugin/events` hosts — the
//! last two interfaces in the doc-53 WIT world. Unlike `wasi:http`/`inference`
//! (concurrent, stream-minting), these are **SYNCHRONOUS, non-streaming** host
//! imports: each func returns a plain `result<…, api-error>`. So they follow the
//! simple `workspace.read` (Option A) pattern — plain `&mut self` host-trait
//! methods over in-memory state behind a permission gate — NOT the
//! `store`/`Accessor` convention. No P3 transport, no streaming.
//!
//! ## Gate as chokepoint
//! [`StorageEventsHost`] carries a `granted: bool`. `storage` is permission-gated
//! (doc 53): when `!granted` every `storage` method returns
//! `api-error{code:"permission-denied"}` before touching any data — the gate is
//! the single chokepoint. `events.emit` is ALWAYS-grantable per doc 53, so it is
//! NOT behind `granted`; it is bounded only by its caps.
//!
//! ## Quotas (doc 49 M5/M3)
//! - storage: a per-plugin cap on key COUNT and TOTAL bytes. A `set` that would
//!   exceed either cap returns `api-error{code:"quota-exceeded"}`. (`rate-limited`
//!   is the sibling code for a future per-call-rate cap; documented here, the
//!   demo enforces the size cap.)
//! - events: a per-plugin cap on the event COUNT (the demo sink size) AND a cap
//!   on a single event's payload size. Past the count cap `emit` returns
//!   `api-error{code:"rate-limited"}`; an oversized payload returns
//!   `api-error{code:"quota-exceeded"}`.
//!
//! ## Production routing (injection points — NOT wired here)
//! - storage → Dart's `plugin_storage` DAO via the `PluginStorageSync` worker
//!   side-channel (host-keyed `(plugin_id, key)`). Swap the in-memory `HashMap`
//!   ops in the `storage::Host` impl for `PluginStorageSync` calls; the gate +
//!   quota stay here as the runtime-side chokepoint.
//! - events → Dart's UI event port. Swap the `Vec<PluginEvent>` push in
//!   `events::Host::emit` for a forward to the host event port; the size + rate
//!   caps stay here.
//!
//! Gated behind the `wasi-http` feature (the same feature the other capability
//! hosts use), so default / `--no-default-features` builds are unaffected.

use std::collections::HashMap;

wasmtime::component::bindgen!({
    path: "wit",
    world: "storage-events-guest",
    // The host stores captured events; the test asserts over them.
    additional_derives: [Clone, PartialEq],
    // All funcs are SYNC (plain `result`), so the default `&mut self` trait
    // shape is exactly what we want — no `imports:` async/store overrides.
});

// The generated module trees for the two interfaces (+ the type-only `types`).
use hellohq::plugin::events::PluginEvent;
use hellohq::plugin::types::ApiError;

/// Per-plugin storage quota: at most this many keys.
pub const STORAGE_MAX_KEYS: usize = 128;
/// Per-plugin storage quota: at most this many total value bytes across all keys.
pub const STORAGE_MAX_TOTAL_BYTES: usize = 256 * 1024;
/// Per-plugin events cap: at most this many events captured in the demo sink.
pub const EVENTS_MAX_COUNT: usize = 256;
/// Per-plugin events cap: at most this many bytes in a single event payload.
pub const EVENT_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// In-memory host state implementing the `storage` + `events` host traits.
///
/// - `granted`: the storage permission gate (the chokepoint). `events` is
///   always-grantable and not behind this flag.
/// - `kv`: the demo storage backing (production → Dart `plugin_storage` DAO).
/// - `events`: the demo event sink (production → Dart UI event port).
#[derive(Default)]
pub struct StorageEventsHost {
    pub granted: bool,
    kv: HashMap<String, Vec<u8>>,
    events: Vec<PluginEvent>,
}

impl StorageEventsHost {
    /// Construct a host with the given storage gate decision and empty backing.
    pub fn new(granted: bool) -> Self {
        StorageEventsHost {
            granted,
            kv: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// The events the demo sink captured, in emit order (test inspection point).
    pub fn captured_events(&self) -> &[PluginEvent] {
        &self.events
    }

    /// Total value bytes currently stored (quota accounting helper).
    fn total_bytes(&self) -> usize {
        self.kv.values().map(|v| v.len()).sum()
    }

    /// Register the `storage` + `events` imports (and the type-only `types`
    /// interface, which has an empty Host trait) into `linker`, backed by this
    /// host type. Mirrors `InferenceHost::add_to_linker`.
    pub fn add_to_linker(
        linker: &mut wasmtime::component::Linker<StorageEventsHost>,
    ) -> wasmtime::Result<()> {
        StorageEventsGuest::add_to_linker::<
            StorageEventsHost,
            wasmtime::component::HasSelf<StorageEventsHost>,
        >(linker, |state| state)
    }
}

/// The storage permission-denied error (the gate chokepoint result).
fn denied() -> ApiError {
    ApiError {
        code: "permission-denied".to_string(),
        message: "storage: capability not granted".to_string(),
    }
}

/// The storage quota-exceeded error (key count or total bytes cap).
fn quota_exceeded(what: &str) -> ApiError {
    ApiError {
        code: "quota-exceeded".to_string(),
        message: format!("storage: {what} quota exceeded"),
    }
}

// ── storage::Host (SYNC, `&mut self`, gated, quota'd) ────────────────────────
//
// Every method checks the gate first (the chokepoint): ungranted calls never
// touch `kv`. Production routes the post-gate body to Dart's `plugin_storage`
// DAO via `PluginStorageSync` (see module docs); the gate + quota stay here.
impl hellohq::plugin::storage::Host for StorageEventsHost {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, ApiError> {
        if !self.granted {
            return Err(denied());
        }
        Ok(self.kv.get(&key).cloned())
    }

    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), ApiError> {
        if !self.granted {
            return Err(denied());
        }
        // Quota: project the post-write state. A new key must fit under the key
        // count cap; the total bytes (minus the displaced old value, plus the
        // new one) must fit under the byte cap.
        let is_new_key = !self.kv.contains_key(&key);
        if is_new_key && self.kv.len() >= STORAGE_MAX_KEYS {
            return Err(quota_exceeded("key count"));
        }
        let old_len = self.kv.get(&key).map(|v| v.len()).unwrap_or(0);
        let projected = self.total_bytes() - old_len + value.len();
        if projected > STORAGE_MAX_TOTAL_BYTES {
            return Err(quota_exceeded("total bytes"));
        }
        self.kv.insert(key, value);
        Ok(())
    }

    fn delete(&mut self, key: String) -> Result<(), ApiError> {
        if !self.granted {
            return Err(denied());
        }
        // Idempotent: deleting an absent key is a no-op success.
        self.kv.remove(&key);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), ApiError> {
        if !self.granted {
            return Err(denied());
        }
        self.kv.clear();
        Ok(())
    }

    fn list_keys(&mut self) -> Result<Vec<String>, ApiError> {
        if !self.granted {
            return Err(denied());
        }
        Ok(self.kv.keys().cloned().collect())
    }
}

// ── events::Host (SYNC, `&mut self`, ALWAYS-grantable, size + rate cap) ───────
//
// `emit` is never behind `granted` (always-grantable per doc 53). It captures the
// event into the demo sink under two caps (doc 49 M3): an oversized single
// payload → `quota-exceeded`; past the sink count cap → `rate-limited`.
// Production routes the captured event to Dart's UI event port (see module docs).
impl hellohq::plugin::events::Host for StorageEventsHost {
    fn emit(&mut self, event: PluginEvent) -> Result<(), ApiError> {
        if event.payload.len() > EVENT_MAX_PAYLOAD_BYTES {
            return Err(ApiError {
                code: "quota-exceeded".to_string(),
                message: "events: payload too large".to_string(),
            });
        }
        if self.events.len() >= EVENTS_MAX_COUNT {
            return Err(ApiError {
                code: "rate-limited".to_string(),
                message: "events: emit rate cap reached".to_string(),
            });
        }
        self.events.push(event);
        Ok(())
    }
}

// `types` is type-only (no functions); bindgen still generates a Host trait for
// it that must be satisfied for the linker (mirrors the other hosts).
impl hellohq::plugin::types::Host for StorageEventsHost {}

// ─── Unit tests (host-state surface, no guest/component plumbing) ────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hellohq::plugin::events::Host as _;
    use hellohq::plugin::storage::Host as _;

    #[test]
    fn add_to_linker_links() {
        // Proves the generated world `add_to_linker` accepts our host impl and
        // links the `storage` + `events` imports (+ the type-only `types`).
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let mut linker = wasmtime::component::Linker::<StorageEventsHost>::new(&engine);
        StorageEventsHost::add_to_linker(&mut linker).expect("add_to_linker must link");
    }

    #[test]
    fn storage_round_trip_granted() {
        let mut h = StorageEventsHost::new(true);
        h.set("greeting".into(), b"hello".to_vec()).unwrap();
        h.set("count".into(), vec![7]).unwrap();
        assert_eq!(h.get("greeting".into()).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(h.list_keys().unwrap().len(), 2);
        h.delete("count".into()).unwrap();
        assert_eq!(h.list_keys().unwrap().len(), 1);
        assert_eq!(h.get("count".into()).unwrap(), None);
        h.clear().unwrap();
        assert!(h.list_keys().unwrap().is_empty());
    }

    #[test]
    fn storage_denied_returns_permission_denied() {
        let mut h = StorageEventsHost::new(false);
        assert_eq!(
            h.set("k".into(), vec![1]).unwrap_err().code,
            "permission-denied"
        );
        assert_eq!(h.get("k".into()).unwrap_err().code, "permission-denied");
        assert_eq!(h.delete("k".into()).unwrap_err().code, "permission-denied");
        assert_eq!(h.clear().unwrap_err().code, "permission-denied");
        assert_eq!(h.list_keys().unwrap_err().code, "permission-denied");
    }

    #[test]
    fn storage_key_count_quota() {
        let mut h = StorageEventsHost::new(true);
        for i in 0..STORAGE_MAX_KEYS {
            h.set(format!("k{i}"), vec![0]).unwrap();
        }
        // One more distinct key trips the count cap.
        assert_eq!(
            h.set("overflow".into(), vec![0]).unwrap_err().code,
            "quota-exceeded"
        );
        // Overwriting an existing key still works (not a new key).
        h.set("k0".into(), vec![1, 2]).unwrap();
    }

    #[test]
    fn storage_total_bytes_quota() {
        let mut h = StorageEventsHost::new(true);
        let big = vec![0u8; STORAGE_MAX_TOTAL_BYTES + 1];
        assert_eq!(h.set("big".into(), big).unwrap_err().code, "quota-exceeded");
    }

    #[test]
    fn events_emit_captures_and_caps() {
        let mut h = StorageEventsHost::new(false); // events ignore `granted`
        h.emit(PluginEvent {
            kind: "ready".into(),
            payload: b"ok".to_vec(),
        })
        .unwrap();
        assert_eq!(h.captured_events().len(), 1);
        assert_eq!(h.captured_events()[0].kind, "ready");
        assert_eq!(h.captured_events()[0].payload, b"ok");

        // Oversized payload → quota-exceeded.
        let big = vec![0u8; EVENT_MAX_PAYLOAD_BYTES + 1];
        assert_eq!(
            h.emit(PluginEvent {
                kind: "x".into(),
                payload: big,
            })
            .unwrap_err()
            .code,
            "quota-exceeded"
        );

        // Past the count cap → rate-limited.
        while h.captured_events().len() < EVENTS_MAX_COUNT {
            h.emit(PluginEvent {
                kind: "k".into(),
                payload: vec![],
            })
            .unwrap();
        }
        assert_eq!(
            h.emit(PluginEvent {
                kind: "over".into(),
                payload: vec![],
            })
            .unwrap_err()
            .code,
            "rate-limited"
        );
    }
}
