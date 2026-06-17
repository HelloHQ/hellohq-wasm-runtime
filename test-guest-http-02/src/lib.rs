// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the wasi:http@0.2 END-TO-END gate proof.
//!
//! Built against the `http02-guest` world (../wit-wasi-02): it imports ONLY
//! `wasi:http/{types,outgoing-handler}@0.2.10` and exports
//! `run(authority, use-https) -> result<u16, u8>`. `run` builds an outgoing GET
//! request to `{scheme}://{authority}/`, calls `outgoing-handler::handle`, polls
//! the returned `future-incoming-response` to completion (the wasi:io/poll
//! `pollable.block()` blocks the SYNC guest while Wasmtime drives the host's
//! async send), and returns the response status on success or an error marker.
//!
//! The host wires `handle` through `GatedHttpHooks`, so the guest itself
//! observes the fetch gate's decision: an allowed request returns `Ok(200)`; a
//! denied one surfaces as `Err(2)` (`HttpRequestDenied`).
//!
//! Error encoding (`Err(u8)`):
//!   1 = `handle()` itself returned a NON-denial `ErrorCode`,
//!   2 = `HttpRequestDenied` (the gate's refusal) — surfaced EITHER from
//!       `handle()` directly OR from the response future, depending on the host;
//!   3 = the response future resolved to a different `ErrorCode`,
//!   4 = the future produced no value (should not happen after `block()`).
//!
//! Note: with `wasmtime-wasi-http@45`'s 0.2 path the gated hooks'
//! `send_request` error surfaces from `handle()` directly (the future is never
//! produced), so the denial mapping is applied to the `handle()` `Err` branch
//! too — a denial is reported as `Err(2)` either way.

wit_bindgen::generate!({
    path: "../wit-wasi-02",
    world: "http02-guest",
    // Pull in the transitively-used wasi:http/types + wasi:io interfaces inline
    // rather than requiring `with` mappings.
    generate_all,
});

use wasi::http::outgoing_handler;
use wasi::http::types::{ErrorCode, Fields, OutgoingRequest, Scheme};

struct Component;

impl Guest for Component {
    fn run(authority: String, use_https: bool) -> Result<u16, u8> {
        let headers = Fields::new();
        let req = OutgoingRequest::new(headers);

        let scheme = if use_https {
            Scheme::Https
        } else {
            Scheme::Http
        };
        req.set_scheme(Some(&scheme)).unwrap();
        req.set_authority(Some(&authority)).unwrap();
        req.set_path_with_query(Some("/")).unwrap();

        // Dispatch. A `handle()`-level Err means the request was rejected before
        // a future was even produced — which is exactly how this host surfaces a
        // gate denial. Map the denial code so it stays distinguishable.
        let fut = match outgoing_handler::handle(req, None) {
            Ok(f) => f,
            Err(ec) => {
                return Err(if matches!(ec, ErrorCode::HttpRequestDenied) {
                    2
                } else {
                    1
                })
            }
        };

        // Block the (sync) guest until the response future is ready; Wasmtime
        // drives the host's async send underneath.
        let pollable = fut.subscribe();
        pollable.block();

        match fut.get() {
            Some(Ok(Ok(resp))) => Ok(resp.status()),
            Some(Ok(Err(ec))) => Err(if matches!(ec, ErrorCode::HttpRequestDenied) {
                2
            } else {
                3
            }),
            _ => Err(4),
        }
    }
}

export!(Component);
