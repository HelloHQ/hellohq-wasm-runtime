// SPDX-License-Identifier: Apache-2.0
//
//! **wasi:http@0.2 guest drives the fetch gate END-TO-END.** A REAL guest
//! component (tests/fixtures/http02_guest.component.wasm) imports ONLY
//! `wasi:http/{types,outgoing-handler}@0.2.10` + `wasi:io/poll@0.2.10`,
//! constructs an outgoing GET request to `{scheme}://{authority}/`, calls
//! `outgoing-handler::handle`, blocks on the response future, and returns the
//! status (`Ok(u16)`) or an error marker (`Err(u8)`).
//!
//! The host wires that `handle` through [`GoGuestState`]'s [`GatedHttpHooks`]
//! (the same chokepoint the Go/JS guests hit): the [`fetch_gate`] runs FIRST
//! (https-only + origin allowlist + SSRF/private-IP block), and only on a pass
//! delegates to an INJECTED canned sender (a ready HTTP 200, no real network).
//! So the GUEST ITSELF observes the gate decision:
//!   - allowed → the canned sender is reached → `Ok(200)`,
//!   - denied  → `send_request` returns `HttpRequestDenied` → the guest's
//!     response future resolves to `Err(ErrorCode::HttpRequestDenied)` → the
//!     guest maps it to `Err(2)`, and the canned sender is NEVER reached.
//!
//! The injected sender flips a shared `reached` flag, so each test asserts BOTH
//! the guest-observed result AND whether the send step ran — proving a denial is
//! refused before any network I/O.
//!
//! Backend: **Cranelift** (loading the portable component compiles it), with the
//! async linker `GoGuestState::add_full_to_linker` registers; driven via
//! `instantiate_async` + `call_async` under `pollster::block_on`. Gated behind
//! `wasi-guests`.
#![cfg(feature = "wasi-guests")]

use hellohq_wasm_runtime::wasi_guests::{canned_status_sender, GoGuestState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

/// The real wasi:http@0.2 guest. Imports `wasi:http/{types,outgoing-handler}` +
/// `wasi:io/poll`; exports `run(authority, use-https) -> result<u16, u8>`.
/// Regen: scripts/regen_probe_guest.sh.
const GUEST: &[u8] = include_bytes!("fixtures/http02_guest.component.wasm");

/// Cranelift engine with the component model enabled. Async support is implied
/// by the async WASI host funcs `add_full_to_linker` registers
/// (`add_to_linker_async`), so the call goes through `instantiate_async` /
/// `call_async`.
fn async_engine() -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    Engine::new(&cfg)
}

/// Drive one case: instantiate the guest with `allowlist` wired into the gated
/// hooks + a canned 200 sender, call `run(authority, https)`, and return the
/// guest's `Result<u16, u8>` plus whether the send step was reached.
async fn run_case_async(
    authority: &str,
    https: bool,
    allowlist: Vec<String>,
) -> wasmtime::Result<(Result<u16, u8>, bool)> {
    let engine = async_engine()?;
    let component = Component::from_binary(&engine, GUEST)?;

    let mut linker = Linker::<GoGuestState>::new(&engine);
    GoGuestState::add_full_to_linker(&mut linker)?;

    let reached = Arc::new(AtomicBool::new(false));
    let state = GoGuestState::with_origins_and_sender(
        true,
        allowlist,
        canned_status_sender(200, reached.clone()),
    );
    let mut store = Store::new(&engine, state);

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(String, bool), (Result<u16, u8>,)>(&mut store, "run")?;
    let (result,) = run
        .call_async(&mut store, (authority.to_string(), https))
        .await?;

    Ok((result, reached.load(Ordering::SeqCst)))
}

/// Sync bridge behind the `#[test]` fns.
fn run_case(authority: &str, https: bool, allowlist: Vec<String>) -> (Result<u16, u8>, bool) {
    pollster::block_on(run_case_async(authority, https, allowlist))
        .unwrap_or_else(|e| panic!("guest run failed for {authority:?} (https={https}): {e:?}"))
}

/// Allowed https origin → the gate passes, the canned sender is reached, and the
/// guest observes HTTP 200.
#[test]
fn allow() {
    let (result, reached) = run_case("api.example.com", true, vec!["api.example.com".to_string()]);
    assert_eq!(
        result,
        Ok(200),
        "allowlisted https must reach the 200 sender"
    );
    assert!(reached, "send step must be reached on allow");
}

/// Non-allowlisted origin → denied by the allowlist; the guest sees
/// `HttpRequestDenied` (Err(2)) and the sender is never reached.
#[test]
fn deny_non_allowlisted() {
    let (result, reached) = run_case(
        "evil.example.com",
        true,
        vec!["api.example.com".to_string()],
    );
    assert_eq!(
        result,
        Err(2),
        "non-allowlisted origin must deny (HttpRequestDenied)"
    );
    assert!(!reached, "send must not be reached on deny");
}

/// SSRF: an allowlisted metadata IP literal → denied by the address check; the
/// guest sees `HttpRequestDenied` and the sender is never reached.
#[test]
fn deny_ssrf_metadata_ip() {
    let (result, reached) = run_case("169.254.169.254", true, vec!["169.254.169.254".to_string()]);
    assert_eq!(result, Err(2), "metadata IP must deny (HttpRequestDenied)");
    assert!(!reached, "send must not be reached on SSRF deny");
}

/// Scheme: http:// (even to an allowlisted host) → denied by the https-only
/// check; the guest sees `HttpRequestDenied` and the sender is never reached.
#[test]
fn deny_http_scheme() {
    let (result, reached) = run_case(
        "api.example.com",
        false,
        vec!["api.example.com".to_string()],
    );
    assert_eq!(result, Err(2), "http scheme must deny (HttpRequestDenied)");
    assert!(!reached, "send must not be reached on scheme deny");
}

/// Empty allowlist → deny-all (the safe default); the guest sees
/// `HttpRequestDenied` and the sender is never reached.
#[test]
fn deny_empty_allowlist() {
    let (result, reached) = run_case("api.example.com", true, vec![]);
    assert_eq!(
        result,
        Err(2),
        "empty allowlist must deny (HttpRequestDenied)"
    );
    assert!(!reached, "send must not be reached on empty-allowlist deny");
}

// ─── Plugin request policy, observed from a real guest ───────────────────────
//
// `run-with-headers` sets request headers before sending and reports the
// response header names it can see. The injected sender records the headers it
// was handed and answers with cookie-setting headers, so one run checks both
// halves of the policy: what may leave the host, and what the guest gets back.

use hellohq_wasm_runtime::wasi_guests::{canned_response_sender, SeenHeaders};

type HeadersResult = Result<(u16, Vec<String>), u8>;

async fn run_with_headers_async(
    headers: &[(&str, &str)],
    response_headers: &[(&str, &str)],
    seen: SeenHeaders,
) -> wasmtime::Result<HeadersResult> {
    let engine = async_engine()?;
    let component = Component::from_binary(&engine, GUEST)?;
    let mut linker = Linker::<GoGuestState>::new(&engine);
    GoGuestState::add_full_to_linker(&mut linker)?;

    let state = GoGuestState::with_origins_and_sender(
        true,
        vec!["api.example.com".to_string()],
        canned_response_sender(
            200,
            response_headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            seen,
        ),
    );
    let mut store = Store::new(&engine, state);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(String, Vec<(String, String)>), (HeadersResult,)>(
        &mut store,
        "run-with-headers",
    )?;
    let headers = headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    let (result,) = run
        .call_async(&mut store, ("api.example.com".to_string(), headers))
        .await?;
    Ok(result)
}

fn run_with_headers(
    headers: &[(&str, &str)],
    response_headers: &[(&str, &str)],
) -> (HeadersResult, Option<Vec<(String, String)>>) {
    let seen: SeenHeaders = Default::default();
    let result = pollster::block_on(run_with_headers_async(
        headers,
        response_headers,
        seen.clone(),
    ))
    .unwrap_or_else(|e| panic!("guest run failed: {e:?}"));
    let seen = seen.lock().unwrap().clone();
    (result, seen)
}

/// Credential and unknown headers a guest sets never reach the sender; the
/// allowlisted ones do. `Set-Cookie`/`Set-Cookie2` never reach the guest.
#[test]
fn guest_headers_cut_to_allowlist_and_set_cookie_stripped() {
    let (result, seen) = run_with_headers(
        &[
            ("Authorization", "Bearer secret"),
            ("Cookie", "session=secret"),
            ("X-API-Key", "secret"),
            ("CB-ACCESS-SIGN", "secret"),
            ("X-MBX-APIKEY", "secret"),
            // Forbidden by wasmtime-wasi-http itself: the guest's append fails.
            ("Proxy-Authorization", "Basic c2VjcmV0"),
            ("User-Agent", "plugin/1.0"),
            ("X-Request-Id", "abc"),
            ("Accept", "application/json"),
            ("Content-Type", "application/json"),
        ],
        &[
            ("content-type", "application/json"),
            ("set-cookie", "session=abc; HttpOnly"),
            ("set-cookie2", "legacy=1"),
            ("etag", "\"v1\""),
        ],
    );

    let mut seen = seen.expect("an allowed request must reach the sender");
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ],
        "only allowlisted request headers may leave the host"
    );

    let (status, mut names) = result.expect("allowed request must succeed");
    assert_eq!(status, 200);
    names.sort();
    assert_eq!(
        names,
        ["content-type", "etag"],
        "Set-Cookie must not reach the guest"
    );
}

/// Doc 30 §4.6: a request carrying the reserved credential-handle header is
/// refused on this transport — the guest sees `HttpRequestDenied` and the
/// sender is never reached.
#[test]
fn guest_credential_handle_header_refused() {
    let (result, seen) = run_with_headers(
        &[
            ("x-hellohq-credential", "handle-1"),
            ("Accept", "application/json"),
        ],
        &[],
    );
    assert_eq!(
        result,
        Err(2),
        "credential handle must be refused (HttpRequestDenied)"
    );
    assert!(seen.is_none(), "send must not be reached");
}
