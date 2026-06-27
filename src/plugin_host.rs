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
//! proved the encode/decode in isolation; here it runs over the real P3
//! transport, driven by `hwr_p3_start_workspace_compile` + the poll/resolve C ABI
//! (`tests/plugin_host_p3.rs`).
//!
//! Slice scope: `read-portfolio-names` is wired end-to-end (the canonical first
//! import); the other `workspace` reads follow the identical bridge and return a
//! `not-found` api-error until later slices wire their JSON shapes.

use crate::P3Event;

wasmtime::component::bindgen!({
    path: "wit",
    world: "workspace-run-guest",
});

use hellohq::plugin::types::{
    AggregatedSummary, ApiError, AssetCount, CurrencyRate, PortfolioName, SheetSummary,
};

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

    /// Encode a no-argument typed read as the app's JSON host-call wire and
    /// forward it.
    fn forward_method(&self, method: &str) -> Result<Vec<u8>, ApiError> {
        let request =
            serde_json::to_vec(&serde_json::json!({ "method": method })).map_err(|e| ApiError {
                code: "bad-request".to_string(),
                message: e.to_string(),
            })?;
        self.call(request)
    }
}

fn transport_gone() -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: "host transport closed before the call was serviced".to_string(),
    }
}

fn not_wired(method: &str) -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: format!("{method}: not wired in this C1 slice (read-portfolio-names only)"),
    }
}

/// Decode the app's `{"ok":true,"data":[{"id","name"}]}` /
/// `{"ok":false,"error":…}` host-call reply into the typed
/// `result<list<portfolio-name>, api-error>` the guest receives.
fn decode_portfolio_names(resp: &[u8]) -> Result<Vec<PortfolioName>, ApiError> {
    let v: serde_json::Value = serde_json::from_slice(resp).map_err(|e| ApiError {
        code: "bad-response".to_string(),
        message: e.to_string(),
    })?;
    if v.get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let names = v
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|item| PortfolioName {
                        id: item
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(names)
    } else {
        // The app reports denial as `{"ok":false,"error":"denied:<perm>"}`; map it
        // onto the typed `api-error` the WIT contract returns to the guest.
        let error = v
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("denied")
            .to_string();
        Err(ApiError {
            code: "permission-denied".to_string(),
            message: error,
        })
    }
}

impl hellohq::plugin::workspace::Host for TransportWorkspaceHost {
    fn read_portfolio_names(&mut self) -> Result<Vec<PortfolioName>, ApiError> {
        let resp = self.forward_method("read:portfolio_names")?;
        decode_portfolio_names(&resp)
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
