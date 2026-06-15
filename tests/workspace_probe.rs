// SPDX-License-Identifier: Apache-2.0
//
//! P2 Option A — typed `list<portfolio-name>` round-trip through a real Wasm
//! component, gated. The host implements the `hellohq:plugin/workspace`
//! `read-portfolio-names` import; the fixture guest component
//! (tests/fixtures/workspace_probe_guest.wasm) calls it from its `read-names`
//! export and hands the list back. This proves a typed `list<record>` marshals
//! host -> guest (import result) AND guest -> host (export result), behind a
//! permission gate, on BOTH backends (Cranelift + Pulley).
//!
//! Gated behind `compile` because compiling the portable component on this host
//! needs Cranelift (the default-feature build; the iOS no-JIT build omits it).
#![cfg(feature = "compile")]

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    path: "wit",
    world: "workspace-probe",
    // So the test can assert the round-tripped records equal what the host sent.
    // (PartialEq only — `Eq` is impossible: sibling records carry `f64` fields.)
    additional_derives: [PartialEq],
});

// The world re-exports `PortfolioName` at the test-module root (via the WIT
// `use types.{portfolio-name}`), so the bare name is already in scope — adding a
// `use` for it would clash. `ApiError` lives only under `types`.
use hellohq::plugin::types::ApiError;

/// The fixture guest component. Imports only `hellohq:plugin/workspace`,
/// exports `read-names`. Regen: scripts/regen_probe_guest.sh.
const GUEST_WASM: &[u8] = include_bytes!("fixtures/workspace_probe_guest.wasm");

/// Store state: holds the gate decision. In production the gate is serviced
/// app-side via the Dart-supplied resolver (P3); here a flag stands in.
struct HostState {
    granted: bool,
}

/// The two portfolios the host returns when the gate grants. The test asserts
/// these exact records survive the host -> guest -> host round-trip.
fn host_portfolios() -> Vec<PortfolioName> {
    vec![
        PortfolioName {
            id: "pf-1".to_string(),
            name: "Growth".to_string(),
        },
        PortfolioName {
            id: "pf-2".to_string(),
            name: "Income".to_string(),
        },
    ]
}

// Implement the generated `workspace` import host trait on the store state.
// `read-portfolio-names` is the gate chokepoint; the other five funcs are
// outside this proof and return an `unimplemented` api-error.
impl hellohq::plugin::workspace::Host for HostState {
    fn read_portfolio_names(&mut self) -> Result<Vec<PortfolioName>, ApiError> {
        if self.granted {
            Ok(host_portfolios())
        } else {
            Err(ApiError {
                code: "permission-denied".to_string(),
                message: "workspace read not granted".to_string(),
            })
        }
    }

    fn read_sheet_structure(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::SheetSummary, ApiError> {
        Err(unimplemented_err())
    }

    fn read_asset_count(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::AssetCount, ApiError> {
        Err(unimplemented_err())
    }

    fn read_currency_rates(
        &mut self,
    ) -> Result<Vec<hellohq::plugin::types::CurrencyRate>, ApiError> {
        Err(unimplemented_err())
    }

    fn read_aggregated_values(
        &mut self,
        _portfolio_id: String,
    ) -> Result<hellohq::plugin::types::AggregatedSummary, ApiError> {
        Err(unimplemented_err())
    }

    fn write_external_file(
        &mut self,
        _filename: String,
        _content: Vec<u8>,
    ) -> Result<(), ApiError> {
        Err(unimplemented_err())
    }
}

// `types` is type-only (no functions); bindgen still generates a Host trait for it.
impl hellohq::plugin::types::Host for HostState {}

fn unimplemented_err() -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: "not implemented in the P2 Option A probe".to_string(),
    }
}

/// Build an engine on the chosen backend (mirrors the crate's `make_engine`).
fn engine(use_pulley: bool) -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    if use_pulley {
        cfg.target("pulley64")?;
    }
    Engine::new(&cfg)
}

/// Instantiate the fixture guest, link the gated host impl, call `read-names`,
/// return the typed list the guest hands back.
fn call_read_names(use_pulley: bool, granted: bool) -> wasmtime::Result<Vec<PortfolioName>> {
    let engine = engine(use_pulley)?;
    let component = Component::from_binary(&engine, GUEST_WASM)?;

    let mut linker = Linker::new(&engine);
    // Generated: wires the `workspace` (and `types`) imports to the Host impls.
    // `HasSelf<_>` because `HostState` itself implements the generated Host
    // traits; the getter is the identity projection `&mut T -> &mut T`.
    WorkspaceProbe::add_to_linker::<_, HasSelf<_>>(&mut linker, |s: &mut HostState| s)?;

    let mut store = Store::new(&engine, HostState { granted });
    let probe = WorkspaceProbe::instantiate(&mut store, &component, &linker)?;
    probe.call_read_names(&mut store)
}

fn assert_round_trip(use_pulley: bool) {
    // Gate grants → the typed list round-trips host -> guest -> host intact.
    let names = call_read_names(use_pulley, true).expect("granted read");
    assert_eq!(names, host_portfolios(), "use_pulley={use_pulley}");
    assert_eq!(names.len(), 2);
    assert_eq!(names[0].id, "pf-1");
    assert_eq!(names[0].name, "Growth");
    assert_eq!(names[1].id, "pf-2");
    assert_eq!(names[1].name, "Income");

    // Gate denies → guest receives Err, returns an empty list. No data crosses.
    let denied = call_read_names(use_pulley, false).expect("denied read");
    assert!(denied.is_empty(), "use_pulley={use_pulley}");
}

#[test]
fn cranelift_typed_list_round_trip() {
    assert_round_trip(false);
}

#[test]
fn pulley_typed_list_round_trip() {
    // The no-JIT iOS backend marshals the typed list too.
    assert_round_trip(true);
}
