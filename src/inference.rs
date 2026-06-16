// SPDX-License-Identifier: Apache-2.0
//! A hand-built `hellohq:plugin/inference` streaming host — the capability this
//! crate exists for ("async-first… stream AI inference"). STRUCTURALLY IDENTICAL
//! to the `wasi:http` host in `src/wasi_http.rs`: a concurrent host method
//! (`inference.complete`) routes the request through the P3 v2 streaming
//! transport and maps the streamed output (token deltas) onto the streaming
//! bridge.
//!
//! - `complete` is `result<stream<string>, api-error>` in the WIT — it MINTS a
//!   `stream<string>` value, so (like `wasi:http`'s `handle`) it needs the
//!   `store`/`Accessor` convention rather than `&mut self`. It is bound as a
//!   *concurrent* import (it returns a stream); `bindgen!` flags it `async`.
//! - The request head + the per-token framing are documented precisely below so
//!   the Dart servicer half can match them byte-for-byte.
//! - The returned stream is produced by [`TokenProducer`] (Item = `String`,
//!   Buffer = `VecBuffer<String>`): each inbound P3 frame is one UTF-8
//!   token-delta string; on inbound close the stream is `Dropped`.
//!
//! Requires `Config::concurrency_support(true)` (to mint the stream). Gated
//! behind the `wasi-http` feature (the same feature the `wasi:http` host uses),
//! so default / `--no-default-features` builds are unaffected.

use std::pin::Pin;
use std::task::{Context, Poll};

wasmtime::component::bindgen!({
    path: "wit",
    world: "inference-guest",
    imports: {
        // `complete` is a SYNC WIT func that nonetheless MINTS a `stream<string>`
        // value — so (exactly like `wasi:http`'s `request::new`) it's flagged
        // `store` (NOT `async`): it receives a `wasmtime::component::Access` (a
        // scoped store borrow) in place of `&mut self`, letting it mint the
        // stream via `StreamReader::new`. `async | store` would generate a
        // concurrent import whose async type-flag mismatches the sync WIT func.
        "hellohq:plugin/inference@0.1.0.complete": store,
        default: async,
    },
});

// The generated module tree for the `hellohq:plugin/inference` interface.
use hellohq::plugin::inference::{ApiError, ChatMessage, InferenceOpts};

/// The P3 v2 streaming channel pair the `complete` host method uses to round-trip
/// the request/token-stream with the caller (Dart). `out` carries the request
/// head OUT to the caller; `inbound` is the caller's token-delta stream — taken
/// (`Option::take`) by `complete` and handed to a [`TokenProducer`]. Mirrors
/// `wasi_http::HttpTransport`.
pub(crate) struct InferenceTransport {
    pub(crate) out: crate::P3sOut,
    pub(crate) inbound: Option<crate::P3sIn>,
}

/// In-memory host state implementing the `hellohq:plugin/inference` host traits.
/// Carries the [`InferenceTransport`]: `complete` frames the request OUT to the
/// caller and builds the returned `stream<string>` from the caller's inbound
/// token frames. Mirrors `wasi_http::WasiHttpHost`.
#[derive(Default)]
pub struct InferenceHost {
    pub(crate) transport: Option<InferenceTransport>,
}

impl InferenceHost {
    /// Construct a host wired to a P3 v2 streaming transport.
    pub(crate) fn with_transport(out: crate::P3sOut, inbound: crate::P3sIn) -> Self {
        InferenceHost {
            transport: Some(InferenceTransport {
                out,
                inbound: Some(inbound),
            }),
        }
    }

    /// Register the `hellohq:plugin/inference` import (and the transitively-used
    /// `types` interface, which is type-only → no host trait) into `linker`,
    /// backed by this host type. Mirrors `WasiHttpHost::add_to_linker`.
    pub fn add_to_linker(
        linker: &mut wasmtime::component::Linker<InferenceHost>,
    ) -> wasmtime::Result<()> {
        InferenceGuest::add_to_linker::<InferenceHost, wasmtime::component::HasSelf<InferenceHost>>(
            linker,
            |state| state,
        )
    }
}

// The inference `Host` trait carries the non-stream-minting surface (none here);
// the work lives on `HostWithStore::complete`, a *static* method on the `D`
// projection taking an `Accessor`. Mirrors `wasi:http`'s `handler`.
impl hellohq::plugin::inference::Host for InferenceHost {}

// The type-only `types` interface has an (empty) `Host` trait that must be
// satisfied for the linker, mirroring `wasi::clocks::types::Host`.
impl hellohq::plugin::types::Host for InferenceHost {}

// `complete` is bound `store` (sync-but-stream-minting), so it's a *static*
// method on the `D` projection taking a `wasmtime::component::Access` (a scoped
// store borrow + host-state access) in place of `&mut self` — exactly like
// `wasi:http`'s `request::new`. It needs no `.await`: it emits the request head
// + end synchronously over the transport and mints the `stream<string>` value;
// the streaming itself happens later, pulled by the guest via [`TokenProducer`].
//
// ── Request-head framing (one OUT frame, a UTF-8 text blob) ──────────────────
// Line 1:            `opts: max-tokens={n} temperature={t-or-default}`
//                    (`temperature` is the f32 if present, else the literal
//                    `default`).
// Then, per message: `{role}\n{content}\n---\n`
// Example for `complete([{role:"user", content:"hello"}], {max-tokens:64})`:
//   `opts: max-tokens=64 temperature=default\nuser\nhello\n---\n`
//
// ── Token framing (inbound, caller → host) ───────────────────────────────────
// Each inbound P3 frame is ONE UTF-8 token-delta string → one `stream<string>`
// element. Inbound close (push_end) → the stream ends.
impl hellohq::plugin::inference::HostWithStore for wasmtime::component::HasSelf<InferenceHost> {
    fn complete<T>(
        mut host: wasmtime::component::Access<T, Self>,
        messages: Vec<ChatMessage>,
        opts: InferenceOpts,
    ) -> Result<wasmtime::component::StreamReader<String>, ApiError> {
        // Build the head, emit it OUT, end the outbound stream, and take the
        // inbound receiver out of the host.
        let inbound = {
            let state = host.get();
            let Some(transport) = state.transport.as_mut() else {
                return Err(ApiError {
                    code: "not-found".to_string(),
                    message: "inference: transport missing".to_string(),
                });
            };

            let temperature = match opts.temperature {
                Some(t) => t.to_string(),
                None => "default".to_string(),
            };
            let mut head = format!(
                "opts: max-tokens={} temperature={}\n",
                opts.max_tokens, temperature
            );
            for m in &messages {
                head.push_str(&m.role);
                head.push('\n');
                head.push_str(&m.content);
                head.push_str("\n---\n");
            }

            transport.out.chunk(head.into_bytes());
            transport.out.end();
            transport.inbound.take()
        };

        let Some(inbound) = inbound else {
            return Err(ApiError {
                code: "not-found".to_string(),
                message: "inference: inbound stream already consumed".to_string(),
            });
        };

        // Mint the `stream<string>` over the inbound token-delta frames. Each
        // inbound frame becomes one stream element (chunk-by-chunk, not
        // buffered). On a mint failure surface an `api-error`.
        wasmtime::component::StreamReader::new(&mut host, TokenProducer { inbound }).map_err(|e| {
            ApiError {
                code: "not-found".to_string(),
                message: format!("inference: failed to mint token stream: {e}"),
            }
        })
    }
}

/// A [`StreamProducer`] that forwards inbound P3 token-delta frames to the
/// guest's `stream<string>` element-by-element. Mirrors
/// `wasi_http::ResponseBodyProducer`, but `Item = String` (one UTF-8 token delta
/// per inbound frame) with a `VecBuffer<String>`.
///
/// [`StreamProducer`]: wasmtime::component::StreamProducer
struct TokenProducer {
    inbound: crate::P3sIn,
}

// Generic over the store data `D` (like the blanket `Vec<T>` impl) so it works
// with whatever store type `complete`'s `Accessor` carries.
impl<D> wasmtime::component::StreamProducer<D> for TokenProducer {
    type Item = String;
    type Buffer = wasmtime::component::VecBuffer<String>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: wasmtime::StoreContextMut<'a, D>,
        mut destination: wasmtime::component::Destination<'a, Self::Item, Self::Buffer>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        use futures_util::Stream;
        use wasmtime::component::StreamResult;

        // Poll the inbound receiver for the next token-delta frame. The receiver
        // registers `cx`'s waker on `Pending`, so we just propagate it.
        match Pin::new(&mut self.inbound).poll_next(cx) {
            // A frame arrived: decode it as one UTF-8 token-delta string and
            // hand it to the guest's read buffer. More frames may follow.
            Poll::Ready(Some(frame)) => {
                let token = String::from_utf8_lossy(&frame).into_owned();
                destination.set_buffer(wasmtime::component::VecBuffer::from(vec![token]));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            // Receiver closed (caller pushed end-of-stream): end the guest stream.
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            // No frame yet; the waker is registered, so just wait.
            Poll::Pending => Poll::Pending,
        }
    }
}

// ─── Unit tests (host-state surface, no guest/Accessor plumbing) ─────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_to_linker_links() {
        // Proves the generated world `add_to_linker` accepts our host impl and
        // links the `inference` import (+ the type-only `types` interface).
        let mut cfg = wasmtime::Config::new();
        cfg.wasm_component_model(true);
        cfg.wasm_component_model_async(true);
        cfg.concurrency_support(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let mut linker = wasmtime::component::Linker::<InferenceHost>::new(&engine);
        InferenceHost::add_to_linker(&mut linker).expect("add_to_linker must link");
    }
}
