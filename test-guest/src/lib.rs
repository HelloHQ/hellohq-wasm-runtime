// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the P2 Option A typed-marshaling proof.
//!
//! Built against the `workspace-probe` world (../wit): it imports ONLY
//! `hellohq:plugin/workspace` and exports `read-names`. The export calls the
//! imported `workspace::read-portfolio-names()` and returns the `Ok` list (empty
//! vec on `Err`) — so the host test can assert a typed `list<portfolio-name>`
//! round-trips host -> guest (import result) -> host (export result), gated.
//!
//! no_std + our own global allocator (dlmalloc) so the guest pulls in NO wasi
//! imports; the host linker provides only `workspace`. Any wasi import would
//! fail instantiation in the host test.
#![no_std]

extern crate alloc;

use alloc::vec::Vec;

// Global allocator so `String`/`Vec` work without wasi/libc on
// wasm32-unknown-unknown.
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

// no_std on wasm32-unknown-unknown needs an explicit panic handler.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    path: "../wit",
    world: "workspace-probe",
});

struct Component;

// The world export `read-names: func() -> list<portfolio-name>`.
impl Guest for Component {
    fn read_names() -> Vec<PortfolioName> {
        // Call the gated host import. On `Ok`, hand the typed list straight
        // back through the export (round-trip); on `Err` (gate denial) return
        // an empty list so the host can assert the denied path.
        match hellohq::plugin::workspace::read_portfolio_names() {
            Ok(names) => names,
            Err(_) => Vec::new(),
        }
    }
}

export!(Component);
