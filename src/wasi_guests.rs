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
//! host; an empty allowlist denies everything (the safe default). It then
//! applies the app's plugin request policy ([`crate::request_policy`]): request
//! headers cut to the allowlist, `Set-Cookie` stripped from responses, no
//! redirects, and no free-form (URL-bearing) text in errors handed to the guest.
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
use crate::request_policy;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::types::{
    HostFutureIncomingResponse, IncomingResponse, OutgoingRequestConfig,
};
use wasmtime_wasi_http::p2::{HttpError, HttpResult, WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_http::WasiHttpCtx;

/// The actual-send step `GatedHttpHooks` calls AFTER the gate passes. Injectable
/// so tests can supply a canned sender (no real network); the production default
/// is [`default_sender`], which delegates to `wasmtime_wasi_http`'s turnkey
/// in-process hyper path (`default_send_request`).
pub type SendFn = Box<
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
/// enforces; there is no auto-redirect to disable on this version. The test
/// `production_sender_does_not_follow_redirects` pins this against a real
/// loopback server, so a wasmtime bump that starts following redirects fails CI.
fn default_sender(
    request: hyper::Request<HyperOutgoingBody>,
    config: OutgoingRequestConfig,
) -> HttpResult<HostFutureIncomingResponse> {
    Ok(wasmtime_wasi_http::p2::default_send_request(
        request, config,
    ))
}

/// Test / embedder **stub** sender support: build a [`SendFn`] that performs NO
/// real network I/O — it flips `reached` to `true` (proving the gate passed and
/// delegated to the send step) and returns a ready `HostFutureIncomingResponse`
/// carrying the given `status` and an empty body.
///
/// Wired via [`GoGuestState::with_origins_and_sender`] so an integration test
/// (or an embedder needing a canned response) can drive a real `wasi:http@0.2`
/// guest end-to-end through [`GatedHttpHooks`] without touching the wire — the
/// gate decision always runs first regardless of the injected sender.
#[doc(hidden)]
pub fn canned_status_sender(
    status: u16,
    reached: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> SendFn {
    use std::sync::atomic::Ordering;
    let mut respond = canned_response_sender(status, Vec::new(), Default::default());
    Box::new(move |req, cfg| {
        reached.store(true, Ordering::SeqCst);
        respond(req, cfg)
    })
}

/// Request headers a [`canned_response_sender`] received, as `(name, value)`
/// pairs (lower-case names, grouped by name) — `None` until the sender is
/// reached.
#[doc(hidden)]
pub type SeenHeaders = std::sync::Arc<std::sync::Mutex<Option<Vec<(String, String)>>>>;

/// Test / embedder **stub** sender: performs NO network I/O. Records the
/// request headers it was handed (after the policy ran) into `seen`, and
/// returns a ready response with `status`, the given `response_headers` and an
/// empty body. Lets a test check, from a real guest, both what reaches the wire
/// and what the guest sees of a response (e.g. that `Set-Cookie` is stripped).
#[doc(hidden)]
pub fn canned_response_sender(
    status: u16,
    response_headers: Vec<(String, String)>,
    seen: SeenHeaders,
) -> SendFn {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty};

    Box::new(move |req, cfg| {
        let headers = req
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(headers);
        let mut builder = hyper::Response::builder().status(status);
        for (name, value) in &response_headers {
            builder = builder.header(name, value);
        }
        let resp = builder
            .body(
                Empty::<Bytes>::new()
                    .map_err(|_| unreachable!())
                    .boxed_unsync(),
            )
            .expect("build canned response");
        Ok(HostFutureIncomingResponse::ready(Ok(Ok(
            IncomingResponse {
                resp,
                worker: None,
                between_bytes_timeout: cfg.between_bytes_timeout,
            },
        ))))
    })
}

/// Gated `wasi:http@0.2` hooks. Every outbound `send_request`:
///   1. extracts scheme + host (authority) from the request URI,
///   2. runs [`fetch_gate::check_request`] against the per-plugin
///      `allowlist` — on denial returns the mapped [`ErrorCode`]
///      (`HttpRequestDenied`) WITHOUT touching the network,
///   3. refuses (`HttpRequestDenied`) a request carrying the reserved
///      credential-handle header (doc 30 §4.6 — this path never carries
///      credentials),
///   4. drops every request header the plugin may not set
///      ([`request_policy::is_allowed_request_header`] — the same allowlist the
///      app's `PluginRequestPolicy` applies on every other path),
///   5. delegates to the injectable `send` (default: the turnkey hyper sender,
///      which does not follow redirects) to perform the real request,
///   6. strips `Set-Cookie`/`Set-Cookie2` from the response and blanks the
///      free-form text in any error before the guest sees it.
///
/// `Set-Cookie` is ALSO declared a forbidden header ([`Self::is_forbidden_header`]),
/// which `wasmtime-wasi-http` applies to response headers and response trailers
/// at the guest boundary whatever the sender returned.
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
    /// Request header values dropped by the policy so far (content-blind: a
    /// count, never names or values) — the analogue of the app's per-origin
    /// dropped-header log line, for an embedder to audit.
    dropped_request_headers: usize,
}

impl GatedHttpHooks {
    /// Build gated hooks with the given origin `allowlist` and the production
    /// (turnkey hyper) sender.
    pub fn new(allowlist: Vec<String>) -> Self {
        Self::with_sender(allowlist, Box::new(default_sender))
    }

    /// Build gated hooks with a custom (e.g. canned, test) sender. The gate
    /// still runs first; `send` is reached only on an allowed request, and the
    /// header policy applies whichever sender is used.
    pub fn with_sender(allowlist: Vec<String>, send: SendFn) -> Self {
        GatedHttpHooks {
            allowlist,
            send,
            dropped_request_headers: 0,
        }
    }

    /// How many plugin request header values the policy has dropped so far.
    pub fn dropped_request_headers(&self) -> usize {
        self.dropped_request_headers
    }

    /// Steps 4–6 of [`GatedHttpHooks`]: header policy, send, response
    /// sanitising. Callers MUST have run the gate (steps 1–3) first; split out
    /// only so a test can drive the production sender against a loopback
    /// server the gate would (rightly) refuse.
    fn send_sanitized(
        &mut self,
        mut request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        self.dropped_request_headers += retain_allowed_request_headers(request.headers_mut());
        let response = (self.send)(request, config).map_err(scrub_http_error)?;
        Ok(sanitize_incoming_response(response))
    }
}

/// Map a gate denial to the closest `wasi:http` `ErrorCode`. All map to
/// `HttpRequestDenied` — the spec's "the request was denied" code — so a guest
/// cannot distinguish an allowlist miss from an SSRF block (no information leak).
fn denial_to_error(_denial: FetchDenial) -> ErrorCode {
    ErrorCode::HttpRequestDenied
}

/// Drop every request header a plugin may not set; returns how many values
/// were dropped. `Host` is dropped too — the turnkey sender re-derives it from
/// the (gate-checked) URI authority after this runs.
fn retain_allowed_request_headers(headers: &mut hyper::HeaderMap) -> usize {
    let disallowed: Vec<hyper::header::HeaderName> = headers
        .keys()
        .filter(|name| !request_policy::is_allowed_request_header(name.as_str()))
        .cloned()
        .collect();
    let mut dropped = 0;
    for name in disallowed {
        dropped += headers.get_all(&name).iter().count();
        headers.remove(&name);
    }
    dropped
}

/// Remove [`request_policy::STRIPPED_RESPONSE_HEADERS`] from response headers.
fn strip_response_headers(headers: &mut hyper::HeaderMap) {
    for name in request_policy::STRIPPED_RESPONSE_HEADERS {
        headers.remove(name);
    }
}

/// Blank the free-form text an `ErrorCode` can carry (an internal message, a
/// DNS rcode, a TLS alert message) so no URL, query or upstream detail reaches
/// the guest — the analogue of the app's URL-free `NetworkErr.message`. The
/// error KIND is kept so a guest can still tell a timeout from a refusal.
fn scrub_error_code(code: ErrorCode) -> ErrorCode {
    use wasmtime_wasi_http::p2::bindings::http::types::{DnsErrorPayload, TlsAlertReceivedPayload};
    match code {
        ErrorCode::InternalError(_) => ErrorCode::InternalError(None),
        ErrorCode::DnsError(p) => ErrorCode::DnsError(DnsErrorPayload {
            rcode: None,
            info_code: p.info_code,
        }),
        ErrorCode::TlsAlertReceived(p) => ErrorCode::TlsAlertReceived(TlsAlertReceivedPayload {
            alert_id: p.alert_id,
            alert_message: None,
        }),
        other => other,
    }
}

/// [`scrub_error_code`] for an error returned straight from the sender. A trap
/// (not an `ErrorCode`) stays a trap: it never reaches the guest as data.
fn scrub_http_error(err: HttpError) -> HttpError {
    match err.downcast_ref() {
        Some(code) => scrub_error_code(code.clone()).into(),
        None => err,
    }
}

/// Apply the response half of the policy to whatever the sender produced:
/// strip cookie-setting headers, and scrub errors (including errors raised
/// later while the guest streams the body). A pending response is wrapped in a
/// task that sanitises it once it resolves; dropping the wrapper (the guest
/// dropped its future) drops — and so aborts — the inner send.
fn sanitize_incoming_response(response: HostFutureIncomingResponse) -> HostFutureIncomingResponse {
    match response {
        HostFutureIncomingResponse::Ready(result) => {
            HostFutureIncomingResponse::ready(sanitize_incoming_result(result))
        }
        HostFutureIncomingResponse::Pending(handle) => {
            HostFutureIncomingResponse::pending(wasmtime_wasi::runtime::spawn(async move {
                sanitize_incoming_result(handle.await)
            }))
        }
        consumed @ HostFutureIncomingResponse::Consumed => consumed,
    }
}

fn sanitize_incoming_result(
    result: wasmtime::Result<Result<IncomingResponse, ErrorCode>>,
) -> wasmtime::Result<Result<IncomingResponse, ErrorCode>> {
    use http_body_util::BodyExt;
    result.map(|inner| match inner {
        Ok(mut incoming) => {
            strip_response_headers(incoming.resp.headers_mut());
            incoming.resp = incoming
                .resp
                .map(|body| body.map_err(scrub_error_code).boxed_unsync());
            Ok(incoming)
        }
        Err(code) => Err(scrub_error_code(code)),
    })
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
        // Doc 30 §4.6: a credential handle on this transport is refused, not
        // silently dropped — this path never leaves Rust and must not gain
        // secrets.
        if request
            .headers()
            .keys()
            .any(|name| request_policy::is_reserved_credential_header(name.as_str()))
        {
            return Err(denial_to_error(FetchDenial::CredentialUnsupportedTransport).into());
        }
        // Allowed: header policy, the (injectable) real send, response policy.
        self.send_sanitized(request, config)
    }

    /// The `wasmtime-wasi-http` defaults plus the cookie-setting response
    /// headers. The runtime removes forbidden names from incoming response
    /// headers and trailers before the guest reads them (and refuses them on
    /// guest-built fields), so `Set-Cookie` cannot reach the guest whichever
    /// sender produced the response.
    fn is_forbidden_header(&mut self, name: &hyper::header::HeaderName) -> bool {
        wasmtime_wasi_http::DEFAULT_FORBIDDEN_HEADERS.contains(name)
            || request_policy::is_stripped_response_header(name.as_str())
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

    /// Like [`GoGuestState::with_origins`], but with a **custom (injectable)**
    /// outbound `send` step ([`GatedHttpHooks::with_sender`]). The fetch gate
    /// still runs first against `allowlist`; `send` is reached ONLY on a request
    /// the gate allows. Used by the `wasi:http@0.2` end-to-end test (and any
    /// embedder needing a canned sender) to observe the gate decision without
    /// touching the network — pair with [`canned_status_sender`].
    pub fn with_origins_and_sender(granted: bool, allowlist: Vec<String>, send: SendFn) -> Self {
        let ctx = WasiCtxBuilder::new().build();
        GoGuestState {
            table: ResourceTable::new(),
            ctx,
            http_ctx: WasiHttpCtx::new(),
            http_hooks: GatedHttpHooks::with_sender(allowlist, send),
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
    /// the send step without touching the wire. Delegates to the public
    /// [`canned_status_sender`] (parameterized by status), reused by the
    /// `wasi:http@0.2` end-to-end test.
    fn canned_sender(reached: Arc<AtomicBool>) -> SendFn {
        canned_status_sender(200, reached)
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

    // ── Plugin request policy (port of hellohq `PluginRequestPolicy`) ──────

    fn outgoing_with_headers(
        uri: &str,
        headers: &[(&str, &str)],
    ) -> hyper::Request<HyperOutgoingBody> {
        let mut builder = hyper::Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(empty_body()).expect("build outbound request")
    }

    fn allow_example() -> Vec<String> {
        vec!["api.example.com".to_string()]
    }

    /// Resolve a sender's response (ready or pending) on the wasmtime-wasi
    /// tokio runtime, the way the guest-facing `future-incoming-response` does.
    fn resolve(
        response: HostFutureIncomingResponse,
    ) -> wasmtime::Result<Result<IncomingResponse, ErrorCode>> {
        match response {
            HostFutureIncomingResponse::Ready(r) => r,
            HostFutureIncomingResponse::Pending(handle) => wasmtime_wasi::runtime::in_tokio(handle),
            HostFutureIncomingResponse::Consumed => panic!("response already consumed"),
        }
    }

    fn header_names(headers: &hyper::HeaderMap) -> Vec<String> {
        let mut names: Vec<String> = headers.keys().map(|n| n.as_str().to_string()).collect();
        names.sort();
        names
    }

    /// The Dart suite's allowlist + named-credential + host-controlled cases in
    /// one request: only the five allowlisted headers reach the sender.
    #[test]
    fn wasi_http_02_only_allowlisted_request_headers_reach_the_sender() {
        let seen: SeenHeaders = Default::default();
        let mut hooks = GatedHttpHooks::with_sender(
            allow_example(),
            canned_response_sender(200, Vec::new(), seen.clone()),
        );
        let dropped = [
            ("Authorization", "Bearer secret"),
            ("Proxy-Authorization", "Basic c2VjcmV0"),
            ("Cookie", "session=secret"),
            ("X-API-Key", "secret"),
            ("API-Key", "secret"),
            ("API-Sign", "secret"),
            ("CB-ACCESS-KEY", "secret"),
            ("CB-ACCESS-SIGN", "secret"),
            ("CB-ACCESS-TIMESTAMP", "1"),
            ("CB-ACCESS-PASSPHRASE", "secret"),
            ("OK-ACCESS-KEY", "secret"),
            ("OK-ACCESS-SIGN", "secret"),
            ("OK-ACCESS-PASSPHRASE", "secret"),
            ("X-MBX-APIKEY", "secret"),
            ("X-Auth-Token", "secret"),
            ("Host", "internal.example"),
            ("User-Agent", "plugin/1.0"),
            ("X-Request-Id", "abc"),
            ("Origin", "https://evil.example"),
        ];
        let kept = [
            ("Accept", "application/json"),
            ("accept-language", "en-GB"),
            ("Content-Type", "application/json"),
            ("IF-NONE-MATCH", "\"etag-1\""),
            ("If-Modified-Since", "Wed, 21 Oct 2015 07:28:00 GMT"),
        ];
        let all: Vec<(&str, &str)> = dropped.iter().chain(kept.iter()).copied().collect();
        let result = hooks.send_request(
            outgoing_with_headers("https://api.example.com/", &all),
            config(),
        );
        assert!(result.is_ok(), "allowlisted origin must pass the gate");

        let mut on_wire = seen.lock().unwrap().clone().expect("sender reached");
        on_wire.sort();
        let mut expected: Vec<(String, String)> = kept
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.to_string()))
            .collect();
        expected.sort();
        assert_eq!(on_wire, expected);
        assert_eq!(hooks.dropped_request_headers(), dropped.len());
    }

    /// A multi-valued disallowed header is dropped whole and counted per value.
    #[test]
    fn wasi_http_02_multi_valued_disallowed_header_dropped_entirely() {
        let seen: SeenHeaders = Default::default();
        let mut hooks = GatedHttpHooks::with_sender(
            allow_example(),
            canned_response_sender(200, Vec::new(), seen.clone()),
        );
        let request = outgoing_with_headers(
            "https://api.example.com/",
            &[("cookie", "a=1"), ("cookie", "b=2"), ("accept", "*/*")],
        );
        assert!(hooks.send_request(request, config()).is_ok());
        assert_eq!(
            seen.lock().unwrap().clone().unwrap(),
            vec![("accept".to_string(), "*/*".to_string())]
        );
        assert_eq!(hooks.dropped_request_headers(), 2);
    }

    /// No headers at all is a no-op (the Dart "empty map" case).
    #[test]
    fn wasi_http_02_no_request_headers_is_a_no_op() {
        let seen: SeenHeaders = Default::default();
        let mut hooks = GatedHttpHooks::with_sender(
            allow_example(),
            canned_response_sender(200, Vec::new(), seen.clone()),
        );
        assert!(hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .is_ok());
        assert_eq!(seen.lock().unwrap().clone().unwrap(), Vec::new());
        assert_eq!(hooks.dropped_request_headers(), 0);
    }

    /// Doc 30 §4.6: the reserved credential-handle header is REFUSED on this
    /// transport (any case), not dropped — and the sender is never reached.
    #[test]
    fn wasi_http_02_credential_handle_header_refused() {
        for name in ["x-hellohq-credential", "X-HelloHQ-Credential"] {
            let reached = Arc::new(AtomicBool::new(false));
            let mut hooks =
                GatedHttpHooks::with_sender(allow_example(), canned_sender(reached.clone()));
            let request = outgoing_with_headers(
                "https://api.example.com/",
                &[(name, "handle-123"), ("accept", "*/*")],
            );
            assert_denied(
                hooks.send_request(request, config()),
                "a credential handle on wasi:http@0.2 must be refused",
            );
            assert!(!reached.load(Ordering::SeqCst), "send must not be reached");
        }
    }

    /// The Dart `sanitizeResponseHeaders` case on a READY response.
    #[test]
    fn wasi_http_02_set_cookie_stripped_from_ready_response() {
        let mut hooks = GatedHttpHooks::with_sender(
            allow_example(),
            canned_response_sender(
                200,
                vec![
                    ("content-type".into(), "application/json".into()),
                    ("set-cookie".into(), "session=abc; HttpOnly".into()),
                    ("Set-Cookie2".into(), "legacy=1".into()),
                    ("etag".into(), "\"v1\"".into()),
                ],
                Default::default(),
            ),
        );
        let response = hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .expect("allowed");
        let incoming = resolve(response).unwrap().unwrap();
        assert_eq!(
            header_names(incoming.resp.headers()),
            ["content-type", "etag"]
        );
    }

    /// The same on a PENDING response (the production sender's shape): the
    /// wrapper task strips the headers once the send resolves.
    #[test]
    fn wasi_http_02_set_cookie_stripped_from_pending_response() {
        let send: SendFn = Box::new(|_req, cfg| {
            let between = cfg.between_bytes_timeout;
            Ok(HostFutureIncomingResponse::pending(
                wasmtime_wasi::runtime::spawn(async move {
                    let resp = hyper::Response::builder()
                        .status(200)
                        .header("set-cookie", "session=abc")
                        .header("x-ok", "1")
                        .body(empty_body())
                        .unwrap();
                    Ok(Ok(IncomingResponse {
                        resp,
                        worker: None,
                        between_bytes_timeout: between,
                    }))
                }),
            ))
        });
        let mut hooks = GatedHttpHooks::with_sender(allow_example(), send);
        let response = hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .expect("allowed");
        let incoming = resolve(response).unwrap().unwrap();
        assert_eq!(header_names(incoming.resp.headers()), ["x-ok"]);
    }

    /// `Set-Cookie` is also a forbidden header, which wasmtime-wasi-http
    /// applies to response headers + trailers at the guest boundary; the
    /// library defaults stay forbidden too.
    #[test]
    fn forbidden_headers_add_set_cookie_to_the_defaults() {
        let mut hooks = GatedHttpHooks::new(Vec::new());
        for name in ["set-cookie", "set-cookie2"] {
            assert!(hooks.is_forbidden_header(&hyper::header::HeaderName::from_static(name)));
        }
        for name in wasmtime_wasi_http::DEFAULT_FORBIDDEN_HEADERS.iter() {
            assert!(hooks.is_forbidden_header(name), "{name}");
        }
        for name in ["etag", "content-type", "location"] {
            assert!(!hooks.is_forbidden_header(&hyper::header::HeaderName::from_static(name)));
        }
    }

    const LEAKY: &str = "https://api.example.com/v1?apikey=SECRET";

    /// No URL / query / free text in an error the sender returns directly.
    #[test]
    fn wasi_http_02_sender_error_text_is_scrubbed() {
        let send: SendFn =
            Box::new(|_req, _cfg| Err(ErrorCode::InternalError(Some(LEAKY.to_string())).into()));
        let mut hooks = GatedHttpHooks::with_sender(allow_example(), send);
        let err = hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .expect_err("sender error must surface");
        assert!(matches!(
            err.downcast_ref(),
            Some(ErrorCode::InternalError(None))
        ));
    }

    /// ... nor in an error the response future resolves to ...
    #[test]
    fn wasi_http_02_response_error_text_is_scrubbed() {
        let send: SendFn = Box::new(|_req, _cfg| {
            Ok(HostFutureIncomingResponse::ready(Ok(Err(
                ErrorCode::DnsError(
                    wasmtime_wasi_http::p2::bindings::http::types::DnsErrorPayload {
                        rcode: Some(LEAKY.to_string()),
                        info_code: Some(3),
                    },
                ),
            ))))
        });
        let mut hooks = GatedHttpHooks::with_sender(allow_example(), send);
        let response = hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .expect("allowed");
        match resolve(response).unwrap() {
            Err(ErrorCode::DnsError(p)) => {
                assert_eq!(p.rcode, None);
                assert_eq!(p.info_code, Some(3), "the error kind/code survives");
            }
            other => panic!("expected a scrubbed DnsError, got {other:?}"),
        }
    }

    /// ... nor in an error raised while the guest streams the response body.
    #[test]
    fn wasi_http_02_body_error_text_is_scrubbed() {
        use http_body_util::StreamBody;
        let send: SendFn = Box::new(|_req, cfg| {
            let failing = StreamBody::new(futures_util::stream::iter(vec![Err::<
                hyper::body::Frame<Bytes>,
                ErrorCode,
            >(
                ErrorCode::InternalError(Some(LEAKY.to_string())),
            )]));
            let resp = hyper::Response::builder()
                .status(200)
                .body(failing.boxed_unsync())
                .unwrap();
            Ok(HostFutureIncomingResponse::ready(Ok(Ok(
                IncomingResponse {
                    resp,
                    worker: None,
                    between_bytes_timeout: cfg.between_bytes_timeout,
                },
            ))))
        });
        let mut hooks = GatedHttpHooks::with_sender(allow_example(), send);
        let response = hooks
            .send_request(outgoing("https://api.example.com/"), config())
            .expect("allowed");
        let mut body = resolve(response).unwrap().unwrap().resp.into_body();
        let frame = pollster::block_on(body.frame()).expect("one frame");
        assert!(
            matches!(frame, Err(ErrorCode::InternalError(None))),
            "{frame:?}"
        );
    }

    /// The PRODUCTION sender against a real loopback HTTP/1.1 server: a 302 is
    /// handed back as-is (exactly one request reaches the server — no redirect
    /// is followed), credential headers never reach the wire, the allowlisted
    /// one does, and the response's `Set-Cookie` is stripped. Drives
    /// `send_sanitized` because the gate (rightly) refuses loopback + http.
    #[test]
    fn production_sender_does_not_follow_redirects() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Mutex;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let requests: Arc<Mutex<Vec<String>>> = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let requests = requests.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    requests
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    let reply = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/next\r\n\
                         Set-Cookie: session=abc\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(reply.as_bytes());
                }
            })
        };

        let mut hooks = GatedHttpHooks::new(Vec::new());
        let request = outgoing_with_headers(
            &format!("http://127.0.0.1:{port}/start"),
            &[
                ("authorization", "Bearer secret"),
                ("accept", "application/json"),
            ],
        );
        let plain_http = OutgoingRequestConfig {
            use_tls: false,
            ..config()
        };
        let response = hooks
            .send_sanitized(request, plain_http)
            .expect("send must start");
        let incoming = resolve(response).unwrap().expect("loopback response");
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();

        assert_eq!(incoming.resp.status(), 302, "the 3xx is returned as-is");
        assert_eq!(
            incoming.resp.headers()["location"],
            format!("http://127.0.0.1:{port}/next").as_str()
        );
        assert!(incoming.resp.headers().get("set-cookie").is_none());

        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "no redirect may be followed: {requests:?}"
        );
        let wire = requests[0].to_ascii_lowercase();
        assert!(wire.starts_with("get /start "), "{wire}");
        assert!(wire.contains("accept: application/json"), "{wire}");
        assert!(
            !wire.contains("authorization"),
            "credential on the wire: {wire}"
        );
        assert!(!wire.contains("secret"), "credential on the wire: {wire}");
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
