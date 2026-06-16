// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the SYNCHRONOUS storage + events end-to-end proof (doc
//! 53, the last two interfaces).
//!
//! Built against the `storage-events-guest` world (../wit): it imports ONLY
//! `hellohq:plugin/{storage,events}` (and transitively `hellohq:plugin/types`)
//! and exports `run: func() -> list<u8>`. `run` exercises a realistic sequence:
//!
//!   storage.set("greeting", "hello")
//!   storage.set("count", <bytes>)
//!   storage.get("greeting")        -> expect "hello"
//!   storage.list-keys()            -> expect 2
//!   storage.delete("count")
//!   storage.list-keys()            -> expect 1
//!   events.emit({kind:"ready", payload:"ok"})
//!
//! and returns a compact ASCII summary `"<get-bytes>|<count1>|<count2>"`
//! (e.g. `"hello|2|1"`) so the host test can assert the round-trip. If ANY
//! storage call returns `Err` (e.g. the gate denied case), `run` short-circuits
//! and returns the marker `"ERR:permission-denied"` (the api-error code) so the
//! host can assert the denied path surfaces the error.
//!
//! All imports are SYNC funcs — no stream, no async — so `run` is a plain func.
//! no_std + our own global allocator (dlmalloc) so the guest pulls in NO wasi
//! imports; the host linker provides only `storage` + `events`.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    path: "../wit",
    world: "storage-events-guest",
    // Generate the transitively-used `hellohq:plugin/types` interface inline.
    generate_all,
});

use hellohq::plugin::events::{emit, PluginEvent};
use hellohq::plugin::storage;
use hellohq::plugin::types::ApiError;

struct Component;

/// Build the error marker `"ERR:<code>"` so the host can assert which api-error
/// the gate/quota surfaced (e.g. `ERR:permission-denied`).
fn err_marker(e: ApiError) -> Vec<u8> {
    format!("ERR:{}", e.code).into_bytes()
}

impl Guest for Component {
    fn run() -> Vec<u8> {
        // ── storage round-trip (any Err short-circuits to the marker) ────────
        if let Err(e) = storage::set("greeting", b"hello") {
            return err_marker(e);
        }
        if let Err(e) = storage::set("count", &[7u8]) {
            return err_marker(e);
        }

        let greeting = match storage::get("greeting") {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => return err_marker(e),
        };

        let count1 = match storage::list_keys() {
            Ok(keys) => keys.len(),
            Err(e) => return err_marker(e),
        };

        if let Err(e) = storage::delete("count") {
            return err_marker(e);
        }

        let count2 = match storage::list_keys() {
            Ok(keys) => keys.len(),
            Err(e) => return err_marker(e),
        };

        // ── events.emit (always-grantable; surface its error too) ────────────
        if let Err(e) = emit(&PluginEvent {
            kind: "ready".into(),
            payload: b"ok".to_vec(),
        }) {
            return err_marker(e);
        }

        // Compact summary: "<get-bytes>|<count1>|<count2>", e.g. "hello|2|1".
        let mut out = greeting;
        out.extend_from_slice(format!("|{count1}|{count2}").as_bytes());
        out
    }
}

export!(Component);
