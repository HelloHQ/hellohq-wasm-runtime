// SPDX-License-Identifier: Apache-2.0
//
//! Guest component for the C1 typed-host-over-P3 production proof.
//!
//! Built against the `workspace-run-guest` world (../wit): it imports ONLY the
//! typed `hellohq:plugin/workspace` interface (and transitively `types`) and
//! exports `run: func() -> list<u8>`. `run` calls the TYPED
//! `workspace.read-portfolio-names()` import and returns a compact ASCII summary
//! `"<id>=<name>;<id>=<name>"` (e.g. `"pf-1=Growth;pf-2=Income"`) so the host
//! test can assert the typed list round-tripped. On `Err` it returns the marker
//! `"ERR:<code>"` (e.g. `ERR:permission-denied`) so the denied path is visible.
//!
//! The typed import is a SYNC func, so `run` is a plain func. no_std + our own
//! global allocator (dlmalloc) so the guest pulls in NO wasi imports; the host
//! linker provides only `workspace`.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    path: "../wit",
    world: "workspace-run-guest",
    // Generate the transitively-used `hellohq:plugin/types` interface inline.
    generate_all,
});

use hellohq::plugin::workspace::read_portfolio_names;

struct Component;

impl Guest for Component {
    fn run() -> Vec<u8> {
        match read_portfolio_names() {
            Ok(names) => {
                // "<id>=<name>;<id>=<name>" — proves both fields of each typed
                // record survived the host -> guest marshal.
                let joined = names
                    .iter()
                    .map(|n| format!("{}={}", n.id, n.name))
                    .collect::<Vec<String>>()
                    .join(";");
                joined.into_bytes()
            }
            // The typed `api-error` reached the guest — surface its code.
            Err(e) => format!("ERR:{}", e.code).into_bytes(),
        }
    }
}

export!(Component);
