// SPDX-License-Identifier: Apache-2.0
//
//! Why the last runtime call on this thread failed (hellohq-wasm-runtime#30).
//!
//! The C ABI's start and load entry points return null on failure. Before this
//! module the reason was thrown away, so the app could only say "start failed":
//! a build missing the JIT entitlement looked the same as a broken plugin.
//!
//! Each of those entry points now clears this slot when it starts and, when it
//! fails, sets it to one line the app reads with `hwr_last_error()`:
//!
//! * a stable prefix ([`ERR_JIT_UNAVAILABLE`], [`ERR_ENGINE`], …) the app can
//!   match, like the resource-limit prefixes in [`crate::limits`], then
//! * a fixed sentence written here.
//!
//! **No plugin-supplied text.** A Wasmtime compile or instantiate error can
//! quote the plugin (import names, custom sections), so it is reduced to the
//! fixed sentence for its stage. The one message passed through is the
//! runtime's own `MAP_JIT` refusal (`src/jit_memory.rs`), which carries only a
//! byte count and the OS error.
//!
//! The slot is thread-local, like `errno`: read it on the thread that made the
//! failing call, before making another runtime call there.

use std::cell::RefCell;
use std::ffi::{c_char, CString};

/// The process may not map JIT code memory: on macOS, `MAP_JIT` was refused,
/// which under the hardened runtime means the `allow-jit` entitlement is
/// missing. The rest of the message is the runtime's own `MAP_JIT` error.
pub const ERR_JIT_UNAVAILABLE: &str = "hellohq-runtime-jit-unavailable";
/// The Wasmtime engine could not be created for the requested backend.
pub const ERR_ENGINE: &str = "hellohq-runtime-engine";
/// The bytes are not a Wasm component this runtime can compile.
pub const ERR_INVALID_PLUGIN: &str = "hellohq-plugin-invalid";
/// A precompiled artifact was not built by this runtime version for this
/// backend.
pub const ERR_INCOMPATIBLE_ARTIFACT: &str = "hellohq-plugin-artifact-incompatible";
/// The component could not be instantiated (for example it imports something
/// the host does not provide).
pub const ERR_INSTANTIATE: &str = "hellohq-plugin-instantiate";
/// The caller passed a null pointer where bytes were required.
pub const ERR_BAD_ARGUMENT: &str = "hellohq-runtime-bad-argument";
/// The runtime panicked while starting the plugin.
pub const ERR_INTERNAL: &str = "hellohq-runtime-internal";

thread_local! {
    static LAST: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Where a load failed; picks the fixed sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Engine,
    Compile,
    Deserialize,
    Instantiate,
}

/// Forget the last error on this thread.
pub fn clear() {
    LAST.with(|l| *l.borrow_mut() = None);
}

/// Record `msg` as this thread's last error.
pub fn set(msg: impl Into<String>) {
    let msg = msg.into().replace('\0', " ");
    LAST.with(|l| *l.borrow_mut() = CString::new(msg).ok());
}

/// This thread's last error, if any.
pub fn get() -> Option<String> {
    LAST.with(|l| {
        l.borrow()
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned())
    })
}

/// Pointer to this thread's last error, or null. Valid until the next runtime
/// call on this thread.
pub(crate) fn as_ptr() -> *const c_char {
    LAST.with(|l| l.borrow().as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
}

/// The one-line, plugin-text-free message for a failure at `stage`.
pub fn describe(stage: Stage, err: &wasmtime::Error) -> String {
    // The runtime's own MAP_JIT refusal, wherever it sits in the error chain.
    let full = format!("{err:?}");
    if let Some(at) = full.find(ERR_JIT_UNAVAILABLE) {
        let line = full[at..].lines().next().unwrap_or(ERR_JIT_UNAVAILABLE);
        return line.trim_end().to_string();
    }
    fixed(stage)
}

/// The fixed sentence for `stage`, with its prefix.
pub fn fixed(stage: Stage) -> String {
    match stage {
        Stage::Engine => format!("{ERR_ENGINE}: the runtime engine could not be created"),
        Stage::Compile => {
            format!(
                "{ERR_INVALID_PLUGIN}: the plugin is not a Wasm component this runtime can load"
            )
        }
        Stage::Deserialize => format!(
            "{ERR_INCOMPATIBLE_ARTIFACT}: the precompiled plugin was not built by this runtime \
             version for this backend"
        ),
        Stage::Instantiate => format!("{ERR_INSTANTIATE}: the plugin could not be instantiated"),
    }
}

/// Message for a null pointer argument.
pub fn bad_argument() -> String {
    format!("{ERR_BAD_ARGUMENT}: no component bytes were passed")
}

/// Message for a panic caught at the C ABI boundary.
pub fn internal() -> String {
    format!("{ERR_INTERNAL}: the runtime failed while starting the plugin")
}

/// Record `msg` and return null: the shape every failing start path returns.
pub(crate) fn fail<T>(msg: String) -> *mut T {
    set(msg);
    std::ptr::null_mut()
}

/// Record the message for `err` at `stage` and return null.
pub(crate) fn fail_at<T>(stage: Stage, err: &wasmtime::Error) -> *mut T {
    fail(describe(stage, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_jit_refusal_passes_through_from_anywhere_in_the_chain() {
        let inner = wasmtime::format_err!(
            "{ERR_JIT_UNAVAILABLE}: MAP_JIT allocation of 16384 bytes for Wasm code failed: \
             Operation not permitted (os error 1)"
        );
        let err = inner.context("failed to load the component");
        let msg = describe(Stage::Compile, &err);
        assert!(msg.starts_with(ERR_JIT_UNAVAILABLE), "{msg}");
        assert!(msg.contains("16384 bytes"), "{msg}");
        assert!(!msg.contains("failed to load the component"), "{msg}");
        assert!(!msg.contains('\n'), "{msg}");
    }

    #[test]
    fn any_other_error_is_reduced_to_the_fixed_sentence() {
        let err = wasmtime::format_err!("import `plugin-secret:pkg/iface` not found");
        for stage in [
            Stage::Engine,
            Stage::Compile,
            Stage::Deserialize,
            Stage::Instantiate,
        ] {
            let msg = describe(stage, &err);
            assert_eq!(msg, fixed(stage));
            assert!(!msg.contains("plugin-secret"), "{msg}");
        }
    }

    #[test]
    fn the_slot_is_per_thread_and_clears() {
        set("one");
        assert_eq!(get().as_deref(), Some("one"));
        std::thread::spawn(|| assert_eq!(get(), None))
            .join()
            .unwrap();
        clear();
        assert_eq!(get(), None);
        assert!(as_ptr().is_null());
        set("with\0nul");
        assert_eq!(get().as_deref(), Some("with nul"));
        clear();
    }
}
