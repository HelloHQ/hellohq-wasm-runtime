// SPDX-License-Identifier: Apache-2.0
//
//! C1 keystone — the UNIFIED transport-backed plugin host.
//!
//! `plugin_host.rs` (workspace) and `plugin_host_storage_events.rs` proved each
//! capability bridge with focused, per-interface guests exporting `run()->bytes`.
//! This is the production shape: ONE host implementing ALL the capability traits
//! (`workspace` + `storage` + `events` + `log`) on a single store, driving the
//! canonical `guest` export (`run(input) -> result<bytes,string>`) of a REAL
//! SDK-authored plugin (`capstone_plugin.component.wasm`). Each typed call still
//! forwards over the P3 round-trip as the app's JSON host-call wire, so
//! `servicePluginHostCall` is reused unchanged. This is the entrypoint the app's
//! executor binds (`hwr_p3_start_plugin{,_compile}`), replacing the generic
//! `hostcall.call` path.
//!
//! Scope: the methods the capstone quickstart exercises —
//! `workspace.read-portfolio-names`, `storage.get/set`, `events.emit`,
//! `log.write` — are wired end-to-end. The remaining methods (the other
//! `workspace` reads, `storage.delete/clear/list-keys`) follow the IDENTICAL
//! bridge already proven in the per-capability modules and return a `not-found`
//! api-error until folded in. Gated behind `typed-hosts`.

use crate::P3Event;
use serde_json::{json, Value};

wasmtime::component::bindgen!({
    path: "wit",
    world: "capstone-host",
});

use hellohq::plugin::events::PluginEvent;
use hellohq::plugin::log::Level;
use hellohq::plugin::types::{
    AggregatedSummary, ApiError, AssetCount, CurrencyRate, PortfolioName, SheetSummary,
};

/// Host state: the P3 channel every typed capability call forwards over.
pub struct TransportPluginHost {
    tx: std::sync::mpsc::Sender<P3Event>,
}

fn api_err(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.to_string(),
        message: message.to_string(),
    }
}

fn not_wired(method: &str) -> ApiError {
    api_err(
        "not-found",
        &format!(
            "{method}: not wired in the C1 keystone (follows the proven per-capability bridge)"
        ),
    )
}

fn decode_bytes(data: &Value) -> Vec<u8> {
    data.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.as_u64().map(|x| x as u8))
                .collect()
        })
        .unwrap_or_default()
}

impl TransportPluginHost {
    /// Forward a JSON host-call over the P3 round-trip, blocking (on this worker
    /// thread) until the caller `hwr_p3_resolve`s it. Returns the unwrapped
    /// `data` value, or the typed `api-error` for `{"ok":false,…}`.
    fn forward(&self, request: Value) -> Result<Value, ApiError> {
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

impl hellohq::plugin::workspace::Host for TransportPluginHost {
    fn read_portfolio_names(&mut self) -> Result<Vec<PortfolioName>, ApiError> {
        let data = self.forward(json!({ "method": "read:portfolio_names" }))?;
        Ok(data
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|i| PortfolioName {
                        id: i
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: i
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn read_sheet_structure(&mut self, _portfolio_id: String) -> Result<SheetSummary, ApiError> {
        Err(not_wired("read-sheet-structure"))
    }
    fn read_asset_count(&mut self, _portfolio_id: String) -> Result<AssetCount, ApiError> {
        Err(not_wired("read-asset-count"))
    }
    fn read_currency_rates(&mut self) -> Result<Vec<CurrencyRate>, ApiError> {
        Err(not_wired("read-currency-rates"))
    }
    fn read_aggregated_values(
        &mut self,
        _portfolio_id: String,
    ) -> Result<AggregatedSummary, ApiError> {
        Err(not_wired("read-aggregated-values"))
    }
    fn write_external_file(
        &mut self,
        _filename: String,
        _content: Vec<u8>,
    ) -> Result<(), ApiError> {
        Err(not_wired("write-external-file"))
    }
}

impl hellohq::plugin::storage::Host for TransportPluginHost {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, ApiError> {
        let data = self.forward(json!({ "method": "storage_get", "key": key }))?;
        if data.is_null() {
            Ok(None)
        } else {
            Ok(Some(decode_bytes(&data)))
        }
    }

    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), ApiError> {
        self.forward(json!({ "method": "storage_set", "key": key, "value": value }))?;
        Ok(())
    }

    fn delete(&mut self, _key: String) -> Result<(), ApiError> {
        Err(not_wired("storage.delete"))
    }
    fn clear(&mut self) -> Result<(), ApiError> {
        Err(not_wired("storage.clear"))
    }
    fn list_keys(&mut self) -> Result<Vec<String>, ApiError> {
        Err(not_wired("storage.list-keys"))
    }
}

impl hellohq::plugin::events::Host for TransportPluginHost {
    fn emit(&mut self, event: PluginEvent) -> Result<(), ApiError> {
        self.forward(json!({
            "method": "emit_event",
            "name": event.kind,
            "payload": event.payload,
        }))?;
        Ok(())
    }
}

impl hellohq::plugin::log::Host for TransportPluginHost {
    fn write(&mut self, level: Level, message: String) {
        // `log.write` is never gated and has no return — fire-and-forget. Forward
        // best-effort so the app can route it to its log port; ignore the reply.
        let level = match level {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        };
        let _ = self.forward(json!({ "method": "log", "level": level, "message": message }));
    }
}

impl hellohq::plugin::types::Host for TransportPluginHost {}

/// Worker-thread drive for a canonical `guest`-exporting plugin: link the unified
/// transport host (all four capabilities), instantiate the `capstone-host` world,
/// and call the plugin's `guest.run(input)`. Returns the plugin's `run` result
/// (`Ok(bytes)` → `HWR_P3_DONE`, `Err(message)` → `HWR_P3_ERROR`). Synchronous —
/// the host imports block on the caller's resolve — so the caller drives it over
/// the existing P3 poll/resolve C ABI.
pub(crate) fn drive_plugin_run(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    input: Vec<u8>,
    tx: std::sync::mpsc::Sender<P3Event>,
) -> Result<Vec<u8>, String> {
    let e2s = |e: wasmtime::Error| e.to_string();
    let mut linker = wasmtime::component::Linker::new(&engine);
    CapstoneHost::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |s: &mut TransportPluginHost| s,
    )
    .map_err(e2s)?;

    let mut store = wasmtime::Store::new(&engine, TransportPluginHost { tx });
    let plugin = CapstoneHost::instantiate(&mut store, &component, &linker).map_err(e2s)?;
    // Outer Result = wasmtime call error; inner = the plugin's `run` result.
    plugin
        .hellohq_plugin_guest()
        .call_run(&mut store, &input)
        .map_err(e2s)?
}
