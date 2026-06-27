// SPDX-License-Identifier: Apache-2.0
//
//! C1-3b — transport-backed TYPED `storage` + `events` hosts.
//!
//! The companion of `plugin_host.rs` (the typed `workspace` host): the guest
//! imports the TYPED `hellohq:plugin/{storage,events}` interfaces, and each call
//! is forwarded over the P3 round-trip to the app (Dart) as a JSON host-call. The
//! host bridges the WIT types to/from JSON:
//!   • `storage.set(key, value)` -> `{"method":"storage_set","key":…,"value":[u8…]}`
//!   • `storage.get(key)`        -> `{"method":"storage_get","key":…}` ;
//!                                  reply `{"ok":true,"data":[u8…]|null}`
//!   • `storage.delete/clear`    -> `{"method":"storage_delete|storage_clear",…}`
//!   • `storage.list-keys()`     -> reply `{"ok":true,"data":["k1",…]}`
//!   • `events.emit({kind,payload})` -> `{"method":"emit_event","name":…,"payload":[u8…]}`
//!
//! **Bytes are carried as JSON `u8` arrays** (binary-safe, no extra dep) — the
//! typed `list<u8>` contract supersedes the interim string-only `storage_set`.
//! Wiring the app side to this shape is C1-4 (the app cutover); here the contract
//! is proven end-to-end over the real P3 transport against the existing
//! `storage_events_guest` fixture, with the test playing Dart's servicer.
//!
//! Gated behind `typed-hosts`. Driven by `hwr_p3_start_storage_events{,_compile}`.

use crate::P3Event;
use serde_json::{json, Value};

wasmtime::component::bindgen!({
    path: "wit",
    world: "storage-events-guest",
});

use hellohq::plugin::events::PluginEvent;
use hellohq::plugin::types::ApiError;

/// Host state: the P3 channel each typed `storage`/`events` call forwards over.
pub struct TransportStorageEventsHost {
    tx: std::sync::mpsc::Sender<P3Event>,
}

fn api_err(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.to_string(),
        message: message.to_string(),
    }
}

/// Decode a JSON `u8` array (the wire form for `list<u8>`) into bytes.
fn decode_bytes(data: &Value) -> Vec<u8> {
    data.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_u64().map(|x| x as u8))
                .collect()
        })
        .unwrap_or_default()
}

impl TransportStorageEventsHost {
    /// Forward a JSON host-call over the P3 round-trip and block (on this worker
    /// thread) until the caller `hwr_p3_resolve`s it. Returns the unwrapped
    /// `data` value, or the typed `api-error` for `{"ok":false,…}`. Same
    /// single-`block_on`/no-nesting reasoning as the workspace host.
    fn call(&self, request: Value) -> Result<Value, ApiError> {
        let bytes =
            serde_json::to_vec(&request).map_err(|e| api_err("bad-request", &e.to_string()))?;
        let (respond, rx) = futures_channel::oneshot::channel();
        self.tx
            .send(P3Event::HostCall {
                request: bytes,
                respond,
            })
            .map_err(|_| api_err("not-found", "host transport closed"))?;
        let resp =
            pollster::block_on(rx).map_err(|_| api_err("not-found", "host transport closed"))?;
        let v: Value =
            serde_json::from_slice(&resp).map_err(|e| api_err("bad-response", &e.to_string()))?;
        if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(v.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Err(api_err(
                "permission-denied",
                v.get("error").and_then(Value::as_str).unwrap_or("denied"),
            ))
        }
    }
}

impl hellohq::plugin::storage::Host for TransportStorageEventsHost {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, ApiError> {
        let data = self.call(json!({ "method": "storage_get", "key": key }))?;
        if data.is_null() {
            Ok(None)
        } else {
            Ok(Some(decode_bytes(&data)))
        }
    }

    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), ApiError> {
        self.call(json!({ "method": "storage_set", "key": key, "value": value }))?;
        Ok(())
    }

    fn delete(&mut self, key: String) -> Result<(), ApiError> {
        self.call(json!({ "method": "storage_delete", "key": key }))?;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), ApiError> {
        self.call(json!({ "method": "storage_clear" }))?;
        Ok(())
    }

    fn list_keys(&mut self) -> Result<Vec<String>, ApiError> {
        let data = self.call(json!({ "method": "storage_list_keys" }))?;
        Ok(data
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl hellohq::plugin::events::Host for TransportStorageEventsHost {
    fn emit(&mut self, event: PluginEvent) -> Result<(), ApiError> {
        // WIT `kind` maps onto the app's existing `emit_event` `name` field.
        self.call(json!({
            "method": "emit_event",
            "name": event.kind,
            "payload": event.payload,
        }))?;
        Ok(())
    }
}

impl hellohq::plugin::types::Host for TransportStorageEventsHost {}

/// Worker-thread drive for a typed `storage`/`events` guest: link the
/// transport-backed host, instantiate the `storage-events-guest` component, call
/// its `run` export. Synchronous (host imports block on the caller's resolve), so
/// the caller drives it over the existing P3 poll/resolve C ABI.
pub(crate) fn drive_storage_events_run(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    tx: std::sync::mpsc::Sender<P3Event>,
) -> Result<Vec<u8>, String> {
    let e2s = |e: wasmtime::Error| e.to_string();
    let mut linker = wasmtime::component::Linker::new(&engine);
    StorageEventsGuest::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |s: &mut TransportStorageEventsHost| s,
    )
    .map_err(e2s)?;

    let mut store = wasmtime::Store::new(&engine, TransportStorageEventsHost { tx });
    let guest = StorageEventsGuest::instantiate(&mut store, &component, &linker).map_err(e2s)?;
    guest.call_run(&mut store).map_err(e2s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_u8_array_to_bytes() {
        assert_eq!(decode_bytes(&json!([104, 101, 108, 108, 111])), b"hello");
        assert!(decode_bytes(&Value::Null).is_empty());
        assert!(decode_bytes(&json!("nope")).is_empty());
        // Out-of-range / non-integer entries are dropped, not panicked on.
        assert_eq!(decode_bytes(&json!([1, "x", 2])), vec![1u8, 2u8]);
    }
}
