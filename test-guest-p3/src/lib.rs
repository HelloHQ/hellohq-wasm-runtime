// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the P3 host-call round-trip proof.
//!
//! Built against the `p3-probe` world (../wit): it imports ONLY
//! `hellohq:plugin/hostcall` and exports `run`. The export forwards its input
//! through the imported `hostcall::call(input)` and returns the result
//! unchanged — so the host test can assert a `list<u8>` survives the
//! suspend/resume round-trip host -> guest (import) -> host (export).
//!
//! no_std + our own global allocator (dlmalloc) so the guest pulls in NO wasi
//! imports; the host linker provides only `hostcall`. Any wasi import would
//! fail instantiation in the host test.
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

// Global allocator so `Vec` works without wasi/libc on wasm32-unknown-unknown.
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

// no_std on wasm32-unknown-unknown needs an explicit panic handler.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    path: "../wit",
    world: "p3-probe",
});

struct Component;

// The world export `run: func(input: list<u8>) -> list<u8>`.
impl Guest for Component {
    fn run(input: Vec<u8>) -> Vec<u8> {
        // Forward straight through the imported host call and return its
        // result unchanged.
        hellohq::plugin::hostcall::call(&input)
    }
}

export!(Component);
