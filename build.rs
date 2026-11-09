// SPDX-License-Identifier: Apache-2.0
//
// Embeds an Info.plist into the macOS `hellohq-wasm-host` helper. A command-line
// tool needs one (with a bundle identifier) to start under App Sandbox; see
// src/bin/hellohq-wasm-host.Info.plist. Nothing else is built differently.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/bin/hellohq-wasm-host.Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("src/bin/hellohq-wasm-host.Info.plist");
        println!(
            "cargo:rustc-link-arg-bin=hellohq-wasm-host=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
