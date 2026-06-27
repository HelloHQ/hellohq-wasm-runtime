// SPDX-License-Identifier: Apache-2.0
//
//! C1 — production transport-backed TYPED capability hosts.
//!
//! The generic P3 path runs a guest that calls the opaque `hostcall.call`
//! substrate; the app decodes the JSON `{"method":…}` itself. This module runs a
//! guest that imports the TYPED `hellohq:plugin/workspace` interface instead.
//! Each typed call is bridged to the app's EXISTING JSON host-call wire — the
//! host encodes `{"method":"read:portfolio_names"}`, forwards it over the P3
//! round-trip (the worker thread blocks on the caller's `hwr_p3_resolve`), and
//! decodes the `{"ok":true,"data":[…]}` reply back into the typed
//! `list<portfolio-name>` the guest receives.
//!
//! So the guest sees fully typed imports (no `hostcall.call`) while
//! `servicePluginHostCall` is reused UNCHANGED. `tests/workspace_transport.rs`
//! proved the encode/decode in isolation; `tests/plugin_host_p3.rs` runs it over
//! the real P3 transport via `hwr_p3_start_workspace{,_compile}` + the
//! poll/resolve C ABI.
//!
//! Scope: ALL five `workspace` reads are wired over the transport (each
//! `decode_*` is unit-tested against the app's JSON shape; read-portfolio-names
//! is additionally proven end-to-end over P3). `write-external-file` stays
//! fail-closed (RESERVED in the WIT). `storage`/`events`/`log` are the next
//! interfaces to wire (separate guest world); the `inference` streaming path
//! already lives in `inference.rs`.

use crate::P3Event;

wasmtime::component::bindgen!({
    path: "wit",
    world: "workspace-run-guest",
});

use hellohq::plugin::types::{
    AggregatedSummary, ApiError, AssetCount, CategoryCount, CategoryTotal, CurrencyRate,
    PortfolioName, SheetInfo, SheetSummary,
};
use serde_json::Value;

/// Host state for a typed `workspace` run: the P3 channel the typed imports
/// forward over. `read-portfolio-names` becomes a JSON host-call the caller
/// (Dart) services exactly as for the generic path.
pub struct TransportWorkspaceHost {
    tx: std::sync::mpsc::Sender<P3Event>,
}

impl TransportWorkspaceHost {
    /// Forward opaque request bytes over the P3 round-trip and block (on this
    /// worker thread) until the caller `hwr_p3_resolve`s the response. The only
    /// `block_on` on this thread — the worker runs the sync drive directly (not
    /// inside an executor) — so there is no nesting and no deadlock.
    fn call(&self, request: Vec<u8>) -> Result<Vec<u8>, ApiError> {
        let (respond, rx) = futures_channel::oneshot::channel();
        self.tx
            .send(P3Event::HostCall { request, respond })
            .map_err(|_| transport_gone())?;
        pollster::block_on(rx).map_err(|_| transport_gone())
    }

    /// Encode a typed read as the app's JSON host-call wire
    /// (`{"method":…,"portfolio_id":…?}`), forward it, and return the unwrapped
    /// `data` value (or the typed `api-error` for `{"ok":false,…}`). The per-read
    /// `decode_*` helpers shape `data` into the typed WIT record.
    fn forward(&self, method: &str, portfolio_id: Option<&str>) -> Result<Value, ApiError> {
        let mut req = serde_json::json!({ "method": method });
        if let Some(pid) = portfolio_id {
            req["portfolio_id"] = Value::from(pid);
        }
        let request = serde_json::to_vec(&req).map_err(|e| ApiError {
            code: "bad-request".to_string(),
            message: e.to_string(),
        })?;
        let resp = self.call(request)?;
        let v: Value = serde_json::from_slice(&resp).map_err(|e| ApiError {
            code: "bad-response".to_string(),
            message: e.to_string(),
        })?;
        if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            Ok(v.get("data").cloned().unwrap_or(Value::Null))
        } else {
            // The app reports denial as `{"ok":false,"error":"denied:<perm>"}`;
            // map it onto the typed `api-error` the WIT contract returns.
            Err(ApiError {
                code: "permission-denied".to_string(),
                message: v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("denied")
                    .to_string(),
            })
        }
    }
}

// ── JSON `data` -> typed WIT record decoders ─────────────────────────────────
// The app's `servicePluginHostCall` returns `{"ok":true,"data":<X>}`; these map
// `<X>` onto each `workspace` read's WIT result. Keys mirror the WIT record
// fields (snake_case). Pure functions — unit-tested directly below.

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn decode_portfolio_names(data: &Value) -> Vec<PortfolioName> {
    data.as_array()
        .map(|arr| {
            arr.iter()
                .map(|i| PortfolioName {
                    id: s(i, "id"),
                    name: s(i, "name"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decode_sheet_summary(data: &Value) -> SheetSummary {
    SheetSummary {
        portfolio_id: s(data, "portfolio_id"),
        sheets: data
            .get("sheets")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|sheet| SheetInfo {
                        name: s(sheet, "name"),
                        sections: sheet
                            .get("sections")
                            .and_then(Value::as_array)
                            .map(|secs| {
                                secs.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn decode_asset_count(data: &Value) -> AssetCount {
    AssetCount {
        portfolio_id: s(data, "portfolio_id"),
        count_by_category: data
            .get("count_by_category")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|c| CategoryCount {
                        category: s(c, "category"),
                        count: c.get("count").and_then(Value::as_u64).unwrap_or(0) as u32,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn decode_currency_rates(data: &Value) -> Vec<CurrencyRate> {
    data.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| CurrencyRate {
                    id: s(r, "id"),
                    name: s(r, "name"),
                    symbol: s(r, "symbol"),
                    rate: r.get("rate").and_then(Value::as_f64).unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decode_aggregated_summary(data: &Value) -> AggregatedSummary {
    AggregatedSummary {
        portfolio_id: s(data, "portfolio_id"),
        totals: data
            .get("totals")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|t| CategoryTotal {
                        category: s(t, "category"),
                        total: t.get("total").and_then(Value::as_f64).unwrap_or(0.0),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn transport_gone() -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: "host transport closed before the call was serviced".to_string(),
    }
}

impl hellohq::plugin::workspace::Host for TransportWorkspaceHost {
    fn read_portfolio_names(&mut self) -> Result<Vec<PortfolioName>, ApiError> {
        self.forward("read:portfolio_names", None)
            .map(|d| decode_portfolio_names(&d))
    }

    fn read_sheet_structure(&mut self, portfolio_id: String) -> Result<SheetSummary, ApiError> {
        self.forward("read:sheet_structure", Some(&portfolio_id))
            .map(|d| decode_sheet_summary(&d))
    }

    fn read_asset_count(&mut self, portfolio_id: String) -> Result<AssetCount, ApiError> {
        self.forward("read:asset_count", Some(&portfolio_id))
            .map(|d| decode_asset_count(&d))
    }

    fn read_currency_rates(&mut self) -> Result<Vec<CurrencyRate>, ApiError> {
        self.forward("read:currency_rates", None)
            .map(|d| decode_currency_rates(&d))
    }

    fn read_aggregated_values(
        &mut self,
        portfolio_id: String,
    ) -> Result<AggregatedSummary, ApiError> {
        self.forward("read:aggregated_values", Some(&portfolio_id))
            .map(|d| decode_aggregated_summary(&d))
    }

    fn write_external_file(
        &mut self,
        _filename: String,
        _content: Vec<u8>,
    ) -> Result<(), ApiError> {
        // write:external_output is RESERVED in the WIT (no Tier-2 wiring); keep
        // it fail-closed until the app side gains a typed write servicer.
        Err(ApiError {
            code: "not-found".to_string(),
            message: "write-external-file: not wired (reserved capability)".to_string(),
        })
    }
}

impl hellohq::plugin::types::Host for TransportWorkspaceHost {}

/// Worker-thread drive for a typed `workspace` guest: link the transport-backed
/// host, instantiate the `workspace-run-guest` component, and call its `run`
/// export. Synchronous — the host imports block on the caller's resolve — so the
/// caller drives it over the existing P3 poll/resolve C ABI. Returns the guest's
/// byte result (or an error string the session surfaces as `HWR_P3_ERROR`).
pub(crate) fn drive_workspace_run(
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    tx: std::sync::mpsc::Sender<P3Event>,
) -> Result<Vec<u8>, String> {
    let e2s = |e: wasmtime::Error| e.to_string();
    let mut linker = wasmtime::component::Linker::new(&engine);
    WorkspaceRunGuest::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut linker,
        |s: &mut TransportWorkspaceHost| s,
    )
    .map_err(e2s)?;

    let mut store = wasmtime::Store::new(&engine, TransportWorkspaceHost { tx });
    let guest = WorkspaceRunGuest::instantiate(&mut store, &component, &linker).map_err(e2s)?;
    guest.call_run(&mut store).map_err(e2s)
}

// ── Decoder unit tests ───────────────────────────────────────────────────────
// Each `workspace` read's `data` -> typed WIT record mapping, tested directly
// against the JSON shape the app's `servicePluginHostCall` produces. The
// read-portfolio-names path is additionally proven end-to-end over the real P3
// transport in tests/plugin_host_p3.rs.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_portfolio_names() {
        let data = json!([{"id": "pf-1", "name": "Growth"}, {"id": "pf-2", "name": "Income"}]);
        let got = decode_portfolio_names(&data);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "pf-1");
        assert_eq!(got[1].name, "Income");
    }

    #[test]
    fn decodes_sheet_summary() {
        let data = json!({
            "portfolio_id": "pf-1",
            "sheets": [{"name": "Main", "sections": ["Assets", "Debts"]}],
        });
        let got = decode_sheet_summary(&data);
        assert_eq!(got.portfolio_id, "pf-1");
        assert_eq!(got.sheets.len(), 1);
        assert_eq!(got.sheets[0].name, "Main");
        assert_eq!(got.sheets[0].sections, vec!["Assets", "Debts"]);
    }

    #[test]
    fn decodes_asset_count() {
        let data = json!({
            "portfolio_id": "pf-1",
            "count_by_category": [{"category": "equities", "count": 3}],
        });
        let got = decode_asset_count(&data);
        assert_eq!(got.portfolio_id, "pf-1");
        assert_eq!(got.count_by_category.len(), 1);
        assert_eq!(got.count_by_category[0].category, "equities");
        assert_eq!(got.count_by_category[0].count, 3);
    }

    #[test]
    fn decodes_currency_rates() {
        let data = json!([{"id": "USD", "name": "US Dollar", "symbol": "$", "rate": 1.08}]);
        let got = decode_currency_rates(&data);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "USD");
        assert_eq!(got[0].symbol, "$");
        assert!((got[0].rate - 1.08).abs() < f64::EPSILON);
    }

    #[test]
    fn decodes_aggregated_summary() {
        let data = json!({
            "portfolio_id": "pf-1",
            "totals": [{"category": "equities", "total": 45000.5}],
        });
        let got = decode_aggregated_summary(&data);
        assert_eq!(got.portfolio_id, "pf-1");
        assert_eq!(got.totals.len(), 1);
        assert_eq!(got.totals[0].category, "equities");
        assert!((got.totals[0].total - 45000.5).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_or_wrong_typed_fields_default_safely() {
        // A malformed/absent `data` must not panic — fields default empty/zero.
        assert!(decode_portfolio_names(&Value::Null).is_empty());
        assert!(decode_currency_rates(&json!("not-an-array")).is_empty());
        let empty = decode_sheet_summary(&json!({}));
        assert_eq!(empty.portfolio_id, "");
        assert!(empty.sheets.is_empty());
    }
}
