// SPDX-License-Identifier: Apache-2.0
//! The end-to-end **capstone**: a full host harness for an SDK-authored Tier-2
//! plugin component, proving SDK → component → runtime → host all fit on the
//! real Wasmtime runtime.
//!
//! The fixture `tests/fixtures/capstone_plugin.component.wasm` is the
//! `hellohq-plugin-sdk` quickstart example (`plugin-sdk/examples/
//! component-quickstart`), built `wasm32-unknown-unknown --release` then wrapped
//! with `wasm-tools component new`. `wasm-tools component wit` on it shows it
//! tree-shakes down to exactly four sync capability imports — `workspace`
//! (`read-portfolio-names`), `storage` (`get`/`set`), `events` (`emit`), `log`
//! (`write`) — plus the type-only `types`, and exports the canonical `guest`
//! interface (`init`/`run`/`metadata`). NO wasi, NO inference.
//!
//! [`CapstoneHarness`] is ONE host struct implementing all four capability host
//! traits (+ the type-only `types`) over the `capstone-host` world, whose import
//! set matches the component's imports exactly so `add_to_linker` satisfies them
//! and instantiation succeeds:
//!   - `workspace`: returns **canned** portfolio names, behind a `granted` gate
//!     (the chokepoint — ungranted reads return `permission-denied`). The
//!     reserved/unused workspace reads return `not-found` (the host implements
//!     the whole interface the world imports, even though the plugin calls only
//!     `read-portfolio-names`).
//!   - `storage` + `events`: **composed** from [`StorageEventsHost`] — its
//!     in-memory KV + quotas and its event sink, reused verbatim by delegating
//!     the trait methods to the embedded instance.
//!   - `log`: captures each log line into an in-memory sink the test inspects.
//!
//! ## Why no inference
//! `inference.complete` STREAMS (`stream<string>`) and needs an **async**
//! guest `run`, but the canonical `guest.run` is SYNC — so the SDK quickstart
//! (and this capstone) cover only the four sync capabilities. Inference is
//! proven end-to-end separately (the `inference-guest` world + `InferenceHost`
//! in `inference.rs`); we do NOT force it through the sync `guest.run`.
//!
//! Gated behind the `wasi-http` feature (the same feature the other capability
//! hosts use), so default / `--no-default-features` builds are unaffected.

use crate::storage_events::StorageEventsHost;

wasmtime::component::bindgen!({
    path: "wit",
    world: "capstone-host",
    // The host stores captured events (via the composed StorageEventsHost) and
    // log lines; the test asserts over them.
    additional_derives: [Clone, PartialEq],
    // All capability funcs are SYNC (plain `result` / no return), so the default
    // `&mut self` trait shape is exactly what we want — no async/store overrides.
});

use hellohq::plugin::events::PluginEvent;
use hellohq::plugin::log::Level;
use hellohq::plugin::types::{ApiError, PortfolioName};
use hellohq::plugin::workspace::{AggregatedSummary, AssetCount, CurrencyRate, SheetSummary};

/// One captured log line (level + message) — the host's log sink element.
#[derive(Clone, PartialEq, Debug)]
pub struct LogLine {
    pub level: Level,
    pub message: String,
}

/// The single host struct backing the whole `capstone-host` world.
///
/// - `granted`: the permission gate for the gated `workspace` read (the
///   chokepoint). It is forwarded into the composed `StorageEventsHost` so
///   `storage` shares the same gate; `events`/`log` are always-grantable.
/// - `store`: the composed [`StorageEventsHost`] providing `storage` (gated KV +
///   quotas) and `events` (captured sink). Reused verbatim — the capstone does
///   not re-implement storage/events, it delegates to this.
/// - `logs`: the log sink (the demo capture point; production → host log port).
pub struct CapstoneHarness {
    pub granted: bool,
    store: StorageEventsHost,
    logs: Vec<LogLine>,
}

/// Canned workspace portfolio names the granted `read-portfolio-names` returns.
/// Two entries, so the plugin's summary count is `2`.
fn canned_portfolios() -> Vec<PortfolioName> {
    vec![
        PortfolioName {
            id: "p1".to_string(),
            name: "Growth".to_string(),
        },
        PortfolioName {
            id: "p2".to_string(),
            name: "Income".to_string(),
        },
    ]
}

impl CapstoneHarness {
    /// Construct a host with the given gate decision. The same `granted` flows
    /// into the composed `StorageEventsHost`, so the workspace read AND storage
    /// share one chokepoint.
    pub fn new(granted: bool) -> Self {
        CapstoneHarness {
            granted,
            store: StorageEventsHost::new(granted),
            logs: Vec::new(),
        }
    }

    /// The events the composed storage/events host captured, flattened to
    /// `(kind, payload)` so callers assert over a plain shape rather than the
    /// composed world's generated `PluginEvent` type (test inspection).
    pub fn captured_events(&self) -> Vec<(String, Vec<u8>)> {
        self.store
            .captured_events()
            .iter()
            .map(|e| (e.kind.clone(), e.payload.clone()))
            .collect()
    }

    /// A value the plugin persisted via `storage.set` (test inspection).
    pub fn stored_value(&self, key: &str) -> Option<&Vec<u8>> {
        self.store.stored_value(key)
    }

    /// The log lines the plugin emitted via `log.write` (test inspection).
    pub fn captured_logs(&self) -> &[LogLine] {
        &self.logs
    }

    /// Register all four capability imports (+ the type-only `types`) into
    /// `linker`, backed by this host type. Satisfies exactly the component's
    /// import set (workspace + storage + events + log + types). Delegates to the
    /// generated `capstone-host` world `add_to_linker` (`CapstoneHost` is the
    /// bindgen world struct; `CapstoneHarness` is our host-state type).
    pub fn add_to_linker(
        linker: &mut wasmtime::component::Linker<CapstoneHarness>,
    ) -> wasmtime::Result<()> {
        CapstoneHost::add_to_linker::<CapstoneHarness, wasmtime::component::HasSelf<CapstoneHarness>>(
            linker,
            |state| state,
        )
    }

    /// Register the four capability imports into a linker whose store state `T`
    /// is NOT `CapstoneHarness` itself but EMBEDS one (reachable via `get`). This
    /// is what lets a richer store state — e.g. one that also carries a
    /// `wasmtime_wasi::WasiCtx` + `ResourceTable` to satisfy a Go/JS guest's
    /// `wasi:*` imports — provide `hellohq:plugin/*` from the SAME store. The
    /// capability host funcs are all SYNC; linking them into an otherwise-async
    /// linker (WASI uses `add_to_linker_async`) is fine — sync host funcs are
    /// allowed on an async linker.
    pub fn add_to_linker_get<T: Send + 'static>(
        linker: &mut wasmtime::component::Linker<T>,
        get: fn(&mut T) -> &mut CapstoneHarness,
    ) -> wasmtime::Result<()> {
        CapstoneHost::add_to_linker::<T, wasmtime::component::HasSelf<CapstoneHarness>>(linker, get)
    }
}

/// Map the composed `StorageEventsHost` world's `api-error` onto THIS world's
/// `api-error` (structurally identical: a stable `code` + a safe `message`).
/// `bindgen!` generates a distinct type per world, so composing across worlds
/// needs this thin re-pack at the boundary.
fn map_err(e: crate::storage_events::ApiErrorAlias) -> ApiError {
    ApiError {
        code: e.code,
        message: e.message,
    }
}

/// The permission-denied gate result (workspace + storage chokepoint).
fn denied(what: &str) -> ApiError {
    ApiError {
        code: "permission-denied".to_string(),
        message: format!("{what}: capability not granted"),
    }
}

/// The not-found result for the reserved/unused workspace reads the plugin never
/// calls (the host still implements the full imported interface).
fn not_found(what: &str) -> ApiError {
    ApiError {
        code: "not-found".to_string(),
        message: format!("workspace: {what} not provided by the capstone harness"),
    }
}

// ── workspace::Host (SYNC, `&mut self`, gated; canned data) ──────────────────
//
// `read-portfolio-names` is the one read the plugin calls: gated by `granted`
// (the chokepoint — ungranted reads never reach the canned data), returns the
// canned list when granted. The rest of the imported interface is implemented
// (the world imports the whole `workspace`) but unused by the plugin → `not-found`.
impl hellohq::plugin::workspace::Host for CapstoneHarness {
    fn read_portfolio_names(&mut self) -> Result<Vec<PortfolioName>, ApiError> {
        if !self.granted {
            return Err(denied("workspace"));
        }
        Ok(canned_portfolios())
    }

    fn read_sheet_structure(&mut self, _portfolio_id: String) -> Result<SheetSummary, ApiError> {
        Err(not_found("read-sheet-structure"))
    }

    fn read_asset_count(&mut self, _portfolio_id: String) -> Result<AssetCount, ApiError> {
        Err(not_found("read-asset-count"))
    }

    fn read_currency_rates(&mut self) -> Result<Vec<CurrencyRate>, ApiError> {
        Err(not_found("read-currency-rates"))
    }

    fn read_aggregated_values(
        &mut self,
        _portfolio_id: String,
    ) -> Result<AggregatedSummary, ApiError> {
        Err(not_found("read-aggregated-values"))
    }

    fn write_external_file(
        &mut self,
        _filename: String,
        _content: Vec<u8>,
    ) -> Result<(), ApiError> {
        Err(not_found("write-external-file"))
    }
}

// ── storage::Host (delegated to the composed StorageEventsHost) ──────────────
//
// Reuse the proven gated KV + quotas verbatim by forwarding each method to the
// embedded instance (which carries the same `granted` decision).
impl hellohq::plugin::storage::Host for CapstoneHarness {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, ApiError> {
        self.store.kv_get(key).map_err(map_err)
    }

    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), ApiError> {
        self.store.kv_set(key, value).map_err(map_err)
    }

    fn delete(&mut self, key: String) -> Result<(), ApiError> {
        self.store.kv_delete(key).map_err(map_err)
    }

    fn clear(&mut self) -> Result<(), ApiError> {
        self.store.kv_clear().map_err(map_err)
    }

    fn list_keys(&mut self) -> Result<Vec<String>, ApiError> {
        self.store.kv_list_keys().map_err(map_err)
    }
}

// ── events::Host (delegated to the composed StorageEventsHost) ───────────────
//
// Reuse the proven event sink + caps verbatim by re-packing into the composed
// host's delegation point.
impl hellohq::plugin::events::Host for CapstoneHarness {
    fn emit(&mut self, event: PluginEvent) -> Result<(), ApiError> {
        self.store
            .capture_event(event.kind, event.payload)
            .map_err(map_err)
    }
}

// ── log::Host (SYNC, `&mut self`, always available; captured into a sink) ────
//
// `log` is never gated (always available per doc 53). Each line is captured into
// the demo sink (production → host log port).
impl hellohq::plugin::log::Host for CapstoneHarness {
    fn write(&mut self, level: Level, message: String) {
        self.logs.push(LogLine { level, message });
    }
}

// `types` is type-only (no functions); bindgen still generates an empty Host
// trait that must be satisfied for the linker (mirrors the other hosts).
impl hellohq::plugin::types::Host for CapstoneHarness {}

// ─── Unit tests (host-state surface, no guest/component plumbing) ────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hellohq::plugin::log::Host as _;
    use hellohq::plugin::storage::Host as _;
    use hellohq::plugin::workspace::Host as _;

    #[test]
    fn add_to_linker_links() {
        // Proves the generated world `add_to_linker` accepts our host impl and
        // links all four capability imports (+ the type-only `types`).
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let mut linker = wasmtime::component::Linker::<CapstoneHarness>::new(&engine);
        CapstoneHarness::add_to_linker(&mut linker).expect("add_to_linker must link");
    }

    #[test]
    fn workspace_gate_chokepoint() {
        let mut granted = CapstoneHarness::new(true);
        assert_eq!(granted.read_portfolio_names().unwrap().len(), 2);

        let mut denied = CapstoneHarness::new(false);
        assert_eq!(
            denied.read_portfolio_names().unwrap_err().code,
            "permission-denied"
        );
    }

    #[test]
    fn storage_delegates_to_composed_host() {
        let mut h = CapstoneHarness::new(true);
        h.set("greeting".into(), b"hello".to_vec()).unwrap();
        assert_eq!(h.stored_value("greeting"), Some(&b"hello".to_vec()));
        assert_eq!(h.get("greeting".into()).unwrap(), Some(b"hello".to_vec()));

        let mut d = CapstoneHarness::new(false);
        assert_eq!(
            d.set("k".into(), vec![1]).unwrap_err().code,
            "permission-denied"
        );
    }

    #[test]
    fn log_sink_captures() {
        let mut h = CapstoneHarness::new(true);
        h.write(Level::Info, "hello".into());
        h.write(Level::Debug, "world".into());
        assert_eq!(h.captured_logs().len(), 2);
        assert_eq!(h.captured_logs()[0].level, Level::Info);
        assert_eq!(h.captured_logs()[0].message, "hello");
    }
}
