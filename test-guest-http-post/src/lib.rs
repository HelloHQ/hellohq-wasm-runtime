// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the streaming-REQUEST-body (POST) end-to-end proof.
//!
//! Built against the `http-guest-post` world (../wit-wasi): imports
//! `wasi:http/{types,handler}`, exports `run: func() -> list<u8>`. `run` builds
//! a POST request to `example.com/submit` carrying a body `stream<u8>` (the
//! bytes "req-body-123"), calls `handler.handle`, reads the response status +
//! body stream, and returns the body bytes.
//!
//! The request body is minted via `wit_stream::new::<u8>()`: the *reader* half
//! is handed to `Request::new(headers, Some(reader), …)`; a `spawn_local`'d task
//! writes the body bytes to the *writer* half and then drops it (closing the
//! stream). Because the host only drains the request body while servicing
//! `handle`, the write must run concurrently with the `handle().await` — hence
//! the spawned writer task.

wit_bindgen::generate!({
    path: "../wit-wasi",
    world: "http-guest-post",
    generate_all,
});

use wasi::http::handler::handle;
use wasi::http::types::{ErrorCode, Fields, Method, Request};

struct Component;

impl Guest for Component {
    async fn run() -> Vec<u8> {
        let headers = Fields::new();

        // Mint the request body stream. The reader half goes into the request;
        // a spawned task writes "req-body-123" to the writer half, then drops it
        // (drop closes the stream → host sees end-of-body).
        let (mut body_tx, body_rx) = wit_stream::new::<u8>();

        // Trailers future for the request body: a ready `Ok(None)`.
        let (_req_trailers_tx, req_trailers) =
            wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));

        // Construct a POST request carrying the body stream.
        let (request, _transmit) = Request::new(headers, Some(body_rx), req_trailers, None);
        request.set_method(&Method::Post).unwrap();
        request.set_authority(Some("example.com")).unwrap();
        request.set_path_with_query(Some("/submit")).unwrap();

        // Write the body concurrently with `handle` (the host drains it while
        // servicing the request). `write_all` resolves once the host has read
        // every byte; dropping `body_tx` afterward closes the stream.
        wit_bindgen::spawn_local(async move {
            let _ = body_tx.write_all(b"req-body-123".to_vec()).await;
            // `body_tx` dropped here → request body stream ends.
        });

        let response = match handle(request).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let status = response.get_status_code();

        let (_res_tx, res) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (body_stream, _resp_trailers) =
            wasi::http::types::Response::consume_body(response, res);
        let body = body_stream.collect().await;

        let mut out = Vec::with_capacity(2 + body.len());
        out.extend_from_slice(&status.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }
}

export!(Component);
