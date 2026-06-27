// SPDX-License-Identifier: Apache-2.0
//
//! C1 slice 1 — the TYPED host import, serviced over a Dart round-trip.
//!
//! `tests/workspace_probe.rs` proves the typed `workspace.read-portfolio-names`
//! import marshals host -> guest -> host with an IN-RUST host. Production must
//! instead forward each typed call to the app (Dart), which services it gated.
//! This test proves that bridge: a transport-backed `workspace` host whose typed
//! `read-portfolio-names` ENCODES the call as the app's EXISTING JSON host-call
//! wire (`{"method":"read:portfolio_names"}` -> `{"ok":true,"data":[…]}` — the
//! exact shape `servicePluginHostCall` already speaks), hands it to a `service`
//! closure standing in for the Dart resolver, and DECODES the JSON response back
//! into the typed `list<portfolio-name>` the guest receives.
//!
//! So the guest sees a fully TYPED import (no generic `hostcall.call`), while the
//! app side stays UNCHANGED — its JSON servicer is reused verbatim. The real
//! transport (the P3 oneshot the worker thread awaits) is a later slice; the
//! encode/decode bridge proven here is the reusable core.
//!
//! Gated behind `compile` (instantiating the portable fixture needs Cranelift,
//! same as `workspace_probe.rs`); runs on both backends.
#![cfg(feature = "compile")]

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "wit",
    world: "workspace-probe",
    additional_derives: [PartialEq],
});

use hellohq::plugin::types::ApiError;

/// The fixture guest: imports only `hellohq:plugin/workspace`, calls
/// `read-portfolio-names`, re-exports what it got. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/workspace_probe_guest.wasm");

/// Transport-backed host: each typed call is forwarded as the app's JSON wire to
/// `service` (Dart's stand-in) and the JSON reply is decoded back to typed values.
struct TransportWorkspaceHost {
    /// Plays the Dart resolver: JSON request bytes in, JSON response bytes out.
    service: Box<dyn FnMut(Vec<u8>) -> Vec<u8> + Send>,
    /// The JSON requests the typed host emitted — so the test asserts the typed
    /// call produced the exact `{"method":…}` the app servicer expects.
    requests: Vec<String>,
}

impl TransportWorkspaceHost {
    fn forward(&mut self, method: &str) -> Vec<u8> {
        let req = serde_json::to_vec(&serde_json::json!({ "method": method })).unwrap();
        self.requests
            .push(String::from_utf8_lossy(&req).into_owned());
        (self.service)(req)
    }
}

/// Decode the app's `{"ok":true,"data":[{"id","name"}]}` / `{"ok":false,"error":…}`
/// host-call reply into the typed `result<list<portfolio-name>, api-error>`.
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
        let resp = self.forward("read:portfolio_names");
        decode_portfolio_names(&resp)
    }

    // The remaining reads follow the SAME bridge pattern; slice 1 proves the
    // mechanism on `read-portfolio-names`, so the rest return a not-found error.
    fn read_sheet_structure(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::SheetSummary, ApiError> {
        Err(not_wired())
    }
    fn read_asset_count(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::AssetCount, ApiError> {
        Err(not_wired())
    }
    fn read_currency_rates(
        &mut self,
    ) -> Result<Vec<hellohq::plugin::types::CurrencyRate>, ApiError> {
        Err(not_wired())
    }
    fn read_aggregated_values(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::AggregatedSummary, ApiError> {
        Err(not_wired())
    }
    fn write_external_file(
        &mut self,
        _filename: String,
        _content: Vec<u8>,
    ) -> Result<(), ApiError> {
        Err(not_wired())
    }
}

impl hellohq::plugin::types::Host for TransportWorkspaceHost {}

fn not_wired() -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: "not wired in C1 slice 1 (read-portfolio-names only)".to_string(),
    }
}

fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Run the fixture guest with a transport-backed host whose `service` plays Dart,
/// returning (the typed list the guest got, the JSON requests the host emitted).
fn run(
    use_pulley: bool,
    service: impl FnMut(Vec<u8>) -> Vec<u8> + Send + 'static,
) -> wasmtime::Result<(Vec<PortfolioName>, Vec<String>)> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::new(&engine);
    WorkspaceProbe::add_to_linker::<_, HasSelf<_>>(
        &mut linker,
        |s: &mut TransportWorkspaceHost| s,
    )?;

    let mut store = Store::new(
        &engine,
        TransportWorkspaceHost {
            service: Box::new(service),
            requests: Vec::new(),
        },
    );
    let probe = WorkspaceProbe::instantiate(&mut store, &component, &linker)?;
    let names = probe.call_read_names(&mut store)?;
    let requests = store.data().requests.clone();
    Ok((names, requests))
}

fn assert_granted(use_pulley: bool) {
    // Dart (the service closure) services the typed call with the app's JSON
    // success envelope; the typed list must reach the guest intact.
    let (names, requests) = run(use_pulley, |_req| {
        br#"{"ok":true,"data":[{"id":"pf-1","name":"Growth"},{"id":"pf-2","name":"Income"}]}"#
            .to_vec()
    })
    .expect("granted read");

    // The typed import emitted exactly the app's host-call wire.
    assert_eq!(
        requests,
        vec![r#"{"method":"read:portfolio_names"}"#.to_string()],
        "use_pulley={use_pulley}"
    );
    // The JSON `data` decoded back into the typed `list<portfolio-name>`.
    assert_eq!(names.len(), 2, "use_pulley={use_pulley}");
    assert_eq!(names[0].id, "pf-1");
    assert_eq!(names[0].name, "Growth");
    assert_eq!(names[1].id, "pf-2");
    assert_eq!(names[1].name, "Income");
}

fn assert_denied(use_pulley: bool) {
    // Dart denies with the app's `{"ok":false,"error":"denied:…"}`; the guest
    // receives a typed `api-error`, returns an empty list — no data crosses.
    let (names, _requests) = run(use_pulley, |_req| {
        br#"{"ok":false,"error":"denied:read:portfolio_names"}"#.to_vec()
    })
    .expect("denied read");
    assert!(names.is_empty(), "use_pulley={use_pulley}");
}

#[test]
fn cranelift_typed_workspace_over_json_transport() {
    assert_granted(false);
    assert_denied(false);
}

#[test]
fn pulley_typed_workspace_over_json_transport() {
    // The no-JIT iOS backend bridges the typed import <-> JSON wire too.
    assert_granted(true);
    assert_denied(true);
}
