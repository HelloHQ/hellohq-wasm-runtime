// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the request-TRAILERS end-to-end proof.
//!
//! Built against the `http-guest-req-trailers` world (../wit-wasi): imports
//! `wasi:http/{types,handler}`, exports `run: func() -> list<u8>`. `run` builds
//! a GET request to `example.com/` whose REQUEST trailers future resolves to
//! `Ok(Some(fields))` carrying a single entry `x-trace` = "req-trailer-1",
//! calls `handler.handle`, reads the response status + body, and returns the
//! status bytes (LE u16).
//!
//! Unlike the no-trailers guests (which pass a ready `Ok(None)` trailers
//! future), this guest's trailers future yields `Ok(Some(..))` — proving the
//! host drains it and surfaces it OUT on the head as a reserved
//! `x-hellohq-request-trailers` line.

wit_bindgen::generate!({
    path: "../wit-wasi",
    world: "http-guest-req-trailers",
    generate_all,
});

use wasi::http::handler::handle;
use wasi::http::types::{ErrorCode, Fields, Request};

struct Component;

impl Guest for Component {
    async fn run() -> Vec<u8> {
        let headers = Fields::new();

        // Request trailers future resolving to `Ok(Some(fields))` with a known
        // entry. `wit_future::new` writes its value only when the writer is
        // written (or dropped) — and the host drains this future *while*
        // servicing `handle().await`. So we must write it concurrently with the
        // `handle().await` below (a spawned task), exactly like the POST guest
        // writes its request body. The closure is the fallback default value.
        let (req_trailers_tx, req_trailers) =
            wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));

        // Construct a GET request (no body) carrying the trailers future.
        let (request, _transmit) = Request::new(headers, None, req_trailers, None);
        request.set_authority(Some("example.com")).unwrap();
        request.set_path_with_query(Some("/")).unwrap();

        // Write the trailers value concurrently with `handle` (the host drains
        // the trailers future while servicing the request). The trailer fields
        // carry a known entry `x-trace` = "req-trailer-1".
        wit_bindgen::spawn_local(async move {
            let t = Fields::new();
            t.set("x-trace", &[b"req-trailer-1".to_vec()]).unwrap();
            let _ = req_trailers_tx.write(Ok(Some(t))).await;
        });

        let response = match handle(request).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let status = response.get_status_code();

        let (_res_tx, res) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body_stream, _resp_trailers) =
            wasi::http::types::Response::consume_body(response, res);
        let _body = body_stream.collect().await;

        status.to_le_bytes().to_vec()
    }
}

export!(Component);
