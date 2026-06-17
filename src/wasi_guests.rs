// SPDX-License-Identifier: Apache-2.0
//! Run **JS (jco) / Go (TinyGo) SDK plugin components** on the host — the
//! "support all WASI generations at once" linker.
//!
//! ## Why this exists
//! A Rust no-std guest built with `hellohq-plugin-sdk` tree-shakes down to
//! importing ONLY `hellohq:plugin/*` — the four capability interfaces the
//! [`crate::capstone`] harness satisfies. But the JS and Go SDKs embed a
//! language runtime (a JS engine via jco; the TinyGo runtime), and that runtime
//! ALSO imports a `wasi:*@0.2` surface. The Go quickstart's full import set is:
//!
//! ```text
//! hellohq:plugin/{types,workspace,storage,events,log}@0.1.0   (the capabilities)
//! wasi:cli/{environment,stdin,stdout,stderr}@0.2.0            (the wasi:0.2 surface
//! wasi:clocks/{monotonic-clock,wall-clock}@0.2.0              the TinyGo runtime
//! wasi:filesystem/{types,preopens}@0.2.0                     imports — the host
//! wasi:io/{error,streams}@0.2.0                              MUST satisfy these or
//! wasi:random/random@0.2.0                                   instantiation fails)
//! ```
//!
//! JS (jco) components additionally import `wasi:http@0.2` as part of the engine
//! baseline even when unused — so the host must ALSO register `wasi:http@0.2` (or
//! instantiation fails on the missing import). Outbound is **gated**: a
//! [`GatedHttpHooks`] runs the [`crate::fetch_gate`] (origin allowlist + H4/H5
//! SSRF / private-IP block + https-only) before any real request leaves the
//! host; an empty allowlist denies everything (the safe default).
//!
//! ## "Support all WASI generations at once"
//! One [`wasmtime::component::Linker`] over [`GoGuestState`] registers, without
//! collision (interface identities are versioned, so they coexist):
//!   - **WASI 0.2 runtime interfaces** via `wasmtime-wasi@45`
//!     (`wasi:cli/io/clocks/filesystem/random@0.2.x`), built **LOCKED DOWN** — a
//!     bare [`WasiCtxBuilder`] with NO preopens, NO env, NO inherited stdio, NO
//!     network. Satisfies the language-runtime imports; grants zero ambient
//!     FS/network.
//!   - **WASI 0.2 `wasi:http`** via `wasmtime-wasi-http@45`, **GATED outbound**:
//!     the [`WasiHttpView`] hands the linker a [`GatedHttpHooks`] carrying the
//!     plugin's origin allowlist. Its `send_request` runs [`crate::fetch_gate`]
//!     (https-only + allowlist + SSRF/private-IP block) and, only on a pass,
//!     delegates to the turnkey in-process hyper sender
//!     (`default_send_request`). An EMPTY allowlist denies everything (the
//!     safe default — same effect as the old deny-by-default). The actual send
//!     is INJECTABLE so tests can supply a canned sender (no real network); the
//!     gate decision always runs first regardless.
//!   - **WASI 0.3-rc `wasi:http`** — the hand-built host in [`crate::wasi_http`].
//!     A DIFFERENT interface version (`@0.3.0-rc-...`) from the 0.2 one, so it can
//!     coexist in the same linker — see [`add_full_to_linker`]'s note.
//!   - **`hellohq:plugin@0.1.0`** capabilities — the [`CapstoneHarness`]
//!     (workspace/storage/events/log), reached via the embedded harness.
//!
//! The host must provide the `wasi:*` interfaces — otherwise `instantiate_*`
//! fails on the unsatisfied imports — but the locked-down ctx means the plugin
//! gets NO ambient capability from WASI; its real, granted capabilities stay
//! confined to `hellohq:plugin/*` (gated by [`CapstoneHarness`]).
//!
//! Gated behind the `wasi-guests` feature so default / `--no-default-features`
//! (the iOS no-JIT size budget) are unaffected — `wasmtime-wasi` /
//! `wasmtime-wasi-http` pull heavy deps (tokio, hyper, cap-std).

use crate::capstone::CapstoneHarness;
use crate::fetch_gate::{self, FetchDenial};
use wasmtime::component::{Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::types::{HostFutureIncomingResponse, OutgoingRequestConfig};
use wasmtime_wasi_http::p2::{HttpResult, WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;

/// The actual-send step `GatedHttpHooks` calls AFTER the gate passes. Injectable
/// so tests can supply a canned sender (no real network); the production default
/// is [`default_sender`], which delegates to `wasmtime_wasi_http`'s turnkey
/// in-process hyper path (`default_send_request`).
type SendFn = Box<
    dyn FnMut(
            hyper::Request<HyperOutgoingBody>,
            OutgoingRequestConfig,
        ) -> HttpResult<HostFutureIncomingResponse>
        + Send,
>;

/// Production sender: hand the (already gate-approved) request to the turnkey
/// `wasmtime-wasi-http` hyper path. Note this version of `wasmtime-wasi-http`
/// does NOT follow redirects — `default_send_request_handler` does a single
/// hyper send over one connection, so a 3xx is surfaced to the guest as-is.
/// That is exactly the `followRedirects = false` (H4) behavior the Dart gate
/// enforces; there is no auto-redirect to disable on this version.
fn default_sender(
    request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
) -> HttpResult<HostFutureIncomingResponse> {
    Ok(wasmtime_wasi_http::p2::default_send_request(
        request, config,
    ))
}

/// Gated `wasi:http@0.2` hooks. Every outbound `send_request`:
///   1. extracts scheme + host (authority) from the request URI,
///   2. runs [`fetch_gate::check_request`] against the per-plugin
///      `allowlist` — on denial returns the mapped [`ErrorCode`]
///      (`HttpRequestDenied`) WITHOUT touching the network,
///   3. on a pass, delegates to the injectable `send` (default: the turnkey
///      hyper sender) to perform the real request.
///
/// An EMPTY `allowlist` denies everything (the misconfiguration guard / safe
/// default — same observable effect as the old deny-by-default). The interface
/// stays present so JS components that import `wasi:http@0.2` as part of the
/// engine baseline still instantiate.
pub struct GatedHttpHooks {
    /// The plugin's declared allowed origins (hostnames only, no scheme/path),
    /// sourced from its `network:fetch` permission scope. Empty → deny all.
    allowlist: Vec<String>,
    /// The actual-send step, run only after the gate passes. Injectable for
    /// tests; default = [`default_sender`].
    send: SendFn,
}

impl GatedHttpHooks {
    /// Build gated hooks with the given origin `allowlist` and the production
    /// (turnkey hyper) sender.
    pub fn new(allowlist: Vec<String>) -> Self {
        GatedHttpHooks {
            allowlist,
            send: Box::new(default_sender),
        }
    }

    /// Build gated hooks with a custom (e.g. canned, test) sender. The gate
    /// still runs first; `send` is reached only on an allowed request.
    pub fn with_sender(allowlist: Vec<String>, send: SendFn) -> Self {
        GatedHttpHooks { allowlist, send }
    }
}

/// Map a gate denial to the closest `wasi:http` `ErrorCode`. All map to
/// `HttpRequestDenied` — the spec's "the request was denied" code — so a guest
/// cannot distinguish an allowlist miss from an SSRF block (no information leak).
fn denial_to_error(_denial: FetchDenial) -> ErrorCode {
    ErrorCode::HttpRequestDenied
}

impl wasmtime_wasi_http::p2::WasiHttpHooks for GatedHttpHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let uri = request.uri();
        let scheme = uri.scheme_str().unwrap_or("");
        let host = uri.host().unwrap_or("");
        // THE CHOKEPOINT: gate decision runs first, before any network I/O.
        if let Err(denial) = fetch_gate::check_request(scheme, host, &self.allowlist) {
            return Err(denial_to_error(denial).into());
        }
        // Allowed: delegate to the (injectable) real send.
        (self.send)(request, config)
    }
}

/// `Store<T>` state for a Go/JS guest, holding ALL WASI generations' state at
/// once:
///   - [`ResourceTable`] — shared by every WASI host (0.2 + 0.2-http resources),
///   - [`WasiCtx`] (locked-down) — backs `wasi:cli/io/clocks/filesystem/random@0.2`,
///   - [`WasiHttpCtx`] + [`GatedHttpHooks`] — backs `wasi:http@0.2`, gated,
///   - [`CapstoneHarness`] — backs `hellohq:plugin/*` (gated capabilities).
pub struct GoGuestState {
    table: ResourceTable,
    ctx: WasiCtx,
    http_ctx: WasiHttpCtx,
    http_hooks: GatedHttpHooks,
    /// The capability host — reused verbatim from the Rust capstone. Carries the
    /// `granted` gate decision; `pub` so a caller/test reads back its sinks.
    pub harness: CapstoneHarness,
}

impl GoGuestState {
    /// Build the store state with a **locked-down** WASI ctx, a **gated**
    /// `wasi:http` ctx with an EMPTY origin allowlist (deny-all outbound — the
    /// safe default), and a capability harness carrying the given `granted` gate
    /// decision. Use [`GoGuestState::with_origins`] to grant specific origins.
    ///
    /// The ctx is built from a bare [`WasiCtxBuilder`] with NOTHING added:
    ///   - no `inherit_stdio`/`inherit_env`/`inherit_network`/`inherit_args`,
    ///   - no `preopened_dir` (so `wasi:filesystem/preopens.get-directories`
    ///     returns an empty list — the guest sees no filesystem),
    ///   - no `env` / no sockets.
    ///
    /// The `wasi:*` interfaces are PRESENT (so the language runtime's imports
    /// resolve) but inert — the guest gets no ambient FS/network/stdio. With an
    /// empty allowlist, the `wasi:http` outbound gate refuses every request.
    pub fn new(granted: bool) -> Self {
        Self::with_origins(granted, Vec::new())
    }

    /// Build the store state with the given outbound-fetch origin `allowlist`
    /// (from the plugin's `network:fetch` scope). Hosts on the allowlist that
    /// also pass the https-only + SSRF checks ([`crate::fetch_gate`]) are
    /// allowed through; everything else is denied. Otherwise identical to
    /// [`GoGuestState::new`].
    pub fn with_origins(granted: bool, allowlist: Vec<String>) -> Self {
        let ctx = WasiCtxBuilder::new().build();
        GoGuestState {
            table: ResourceTable::new(),
            ctx,
            http_ctx: WasiHttpCtx::new(),
            http_hooks: GatedHttpHooks::new(allowlist),
            harness: CapstoneHarness::new(granted),
        }
    }

    /// Wire **all WASI generations + the custom capabilities** into one linker:
    ///   1. `wasmtime_wasi::p2::add_to_linker_async` → every `wasi:*@0.2` runtime
    ///      interface (cli/io/clocks/filesystem/random), async variant.
    ///   2. `wasmtime_wasi_http::p2::add_only_http_to_linker_async` → `wasi:http@0.2`
    ///      (gated outbound). We use the `add_only_http_*` variant — NOT
    ///      `add_to_linker_async` — because step 1 already registered the shared
    ///      `wasi:cli`/`wasi:io`/`wasi:clocks` proxy interfaces; the full
    ///      `add_to_linker_async` would re-register them and collide.
    ///   3. [`CapstoneHarness::add_to_linker_get`] → `hellohq:plugin/*`, reaching
    ///      the embedded harness via the `get` closure. The capability host funcs
    ///      are SYNC; linking them into an otherwise-async linker is fine.
    ///
    /// The 0.3-rc `wasi:http` host ([`crate::wasi_http::WasiHttpHost`]) is a
    /// DIFFERENT interface version (`wasi:http/types@0.3.0-rc-...` vs the 0.2
    /// `wasi:http/types@0.2.x`), so it can be added to this same linker without
    /// colliding — but it requires a different store-state type
    /// (`WasiHttpHost`), so the unified "carry both http generations on one
    /// `GoGuestState`" requires the 0.3 host to be refactored onto a `get`-style
    /// projection like the capstone. Since no current SDK guest imports BOTH 0.2
    /// and 0.3 `wasi:http`, this method wires the 0.2 generation (what the JS/Go
    /// toolchains emit) + the custom capabilities; the 0.3 generation stays
    /// per-instantiation via [`crate::wasi_http::WasiHttpHost::add_to_linker`]
    /// against its own store state. See the module test for the coexistence
    /// check.
    pub fn add_full_to_linker(linker: &mut Linker<Self>) -> wasmtime::Result<()> {
        wasmtime_wasi::p2::add_to_linker_async(linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(linker)?;
        CapstoneHarness::add_to_linker_get(linker, |s: &mut GoGuestState| &mut s.harness)?;
        Ok(())
    }
}

// `wasmtime_wasi` (45) needs ONE trait, `WasiView`, returning a `WasiCtxView`
// bundling `&mut WasiCtx` + `&mut ResourceTable` from the store state.
impl WasiView for GoGuestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

// `wasmtime_wasi_http` (45) needs `WasiHttpView`, returning a `WasiHttpCtxView`
// bundling the http ctx + the SAME resource table + the gated hooks.
impl WasiHttpView for GoGuestState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn async_engine() -> wasmtime::Engine {
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        wasmtime::Engine::new(&cfg).unwrap()
    }

    /// All three generations register on ONE linker without collision: WASI 0.2
    /// runtime interfaces, WASI 0.2 `wasi:http`, and the custom capabilities.
    #[test]
    fn unified_linker_links_all_generations() {
        let engine = async_engine();
        let mut linker = Linker::<GoGuestState>::new(&engine);
        GoGuestState::add_full_to_linker(&mut linker)
            .expect("WASI 0.2 + wasi:http@0.2 + hellohq:plugin/* must link without collision");
    }

    /// The hand-built 0.3-rc `wasi:http` host adds to a linker over ITS OWN store
    /// state without error — confirming the 0.3 generation coexists at the crate
    /// level (different interface version from 0.2). Registering both 0.2 and 0.3
    /// `wasi:http` in ONE linker would need one store type implementing both
    /// views; documented in `add_full_to_linker`.
    #[test]
    fn wasi_http_03_host_links_independently() {
        let engine = async_engine();
        let mut linker = Linker::<crate::wasi_http::WasiHttpHost>::new(&engine);
        crate::wasi_http::WasiHttpHost::add_to_linker(&mut linker)
            .expect("0.3-rc wasi:http host must link on its own store state");
    }

    use bytes::Bytes;
    use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use wasmtime_wasi_http::p2::types::IncomingResponse;
    use wasmtime_wasi_http::p2::WasiHttpHooks;

    fn empty_body() -> UnsyncBoxBody<Bytes, ErrorCode> {
        Empty::<Bytes>::new()
            .map_err(|_| unreachable!())
            .boxed_unsync()
    }

    fn outgoing(uri: &str) -> hyper::Request<HyperOutgoingBody> {
        hyper::Request::builder()
            .uri(uri)
            .body(empty_body())
            .expect("build outbound request")
    }

    fn config() -> OutgoingRequestConfig {
        OutgoingRequestConfig {
            use_tls: true,
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            between_bytes_timeout: Duration::from_secs(1),
        }
    }

    /// A canned sender that flips a flag when reached and returns a ready 200 —
    /// no real network. Lets the allow-path tests prove the gate delegated to
    /// the send step without touching the wire.
    fn canned_sender(reached: Arc<AtomicBool>) -> SendFn {
        Box::new(move |_req, cfg| {
            reached.store(true, Ordering::SeqCst);
            let resp = hyper::Response::builder()
                .status(200)
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|_| unreachable!())
                        .boxed_unsync(),
                )
                .expect("build canned 200 response");
            Ok(HostFutureIncomingResponse::ready(Ok(Ok(
                IncomingResponse {
                    resp,
                    worker: None,
                    between_bytes_timeout: cfg.between_bytes_timeout,
                },
            ))))
        })
    }

    fn assert_denied(result: HttpResult<HostFutureIncomingResponse>, msg: &str) {
        let err = result.err().unwrap_or_else(|| panic!("{msg}"));
        assert!(
            matches!(err.downcast_ref(), Some(ErrorCode::HttpRequestDenied)),
            "{msg}: expected HttpRequestDenied, got a different error"
        );
    }

    /// Empty allowlist → outbound still denied (preserves the old
    /// deny-by-default behavior). The canned sender must NOT be reached.
    #[test]
    fn wasi_http_02_outbound_denied() {
        let reached = Arc::new(AtomicBool::new(false));
        let mut hooks = GatedHttpHooks::with_sender(Vec::new(), canned_sender(reached.clone()));
        let result = hooks.send_request(outgoing("https://example.com/"), config());
        assert_denied(result, "empty allowlist must deny");
        assert!(
            !reached.load(Ordering::SeqCst),
            "send must not be reached on deny"
        );
    }

    /// Allowlisted https origin → the gate PASSES and the hooks reach the send
    /// step (the canned sender returns 200; no real network).
    #[test]
    fn wasi_http_02_allowlisted_passes_gate() {
        let reached = Arc::new(AtomicBool::new(false));
        let allow = vec!["api.example.com".to_string()];
        let mut hooks = GatedHttpHooks::with_sender(allow, canned_sender(reached.clone()));
        let result = hooks.send_request(outgoing("https://api.example.com/"), config());
        assert!(
            result.is_ok(),
            "allowlisted https request must pass the gate"
        );
        assert!(
            reached.load(Ordering::SeqCst),
            "send step must be reached on allow"
        );
    }

    /// Non-allowlisted origin → denied (allowlist miss), send not reached.
    #[test]
    fn wasi_http_02_non_allowlisted_denied() {
        let reached = Arc::new(AtomicBool::new(false));
        let allow = vec!["api.example.com".to_string()];
        let mut hooks = GatedHttpHooks::with_sender(allow, canned_sender(reached.clone()));
        let result = hooks.send_request(outgoing("https://evil.example.com/"), config());
        assert_denied(result, "non-allowlisted origin must deny");
        assert!(!reached.load(Ordering::SeqCst));
    }

    /// SSRF: an allowlisted IP-literal host that is a metadata/private address →
    /// denied by the address check; send not reached.
    #[test]
    fn wasi_http_02_ssrf_literal_denied() {
        let reached = Arc::new(AtomicBool::new(false));
        let allow = vec!["169.254.169.254".to_string()];
        let mut hooks = GatedHttpHooks::with_sender(allow, canned_sender(reached.clone()));
        let result = hooks.send_request(outgoing("https://169.254.169.254/"), config());
        assert_denied(result, "metadata IP must deny");
        assert!(!reached.load(Ordering::SeqCst));
    }

    /// Scheme: http:// (even to an allowlisted host) → denied; send not reached.
    #[test]
    fn wasi_http_02_http_scheme_denied() {
        let reached = Arc::new(AtomicBool::new(false));
        let allow = vec!["api.example.com".to_string()];
        let mut hooks = GatedHttpHooks::with_sender(allow, canned_sender(reached.clone()));
        let result = hooks.send_request(outgoing("http://api.example.com/"), config());
        assert_denied(result, "http scheme must deny");
        assert!(!reached.load(Ordering::SeqCst));
    }

    /// The store's `WasiHttpView` hands the linker the gated hooks carrying the
    /// configured allowlist — end-to-end of the store wiring (empty → deny).
    #[test]
    fn store_wires_gated_hooks() {
        let mut state = GoGuestState::with_origins(true, Vec::new());
        let hooks = state.http().hooks as *mut dyn WasiHttpHooks;
        // SAFETY: `state` outlives this call; the pointer is the live hooks.
        let result = unsafe { (*hooks).send_request(outgoing("https://example.com/"), config()) };
        assert_denied(result, "store-wired empty allowlist must deny");
    }
}
