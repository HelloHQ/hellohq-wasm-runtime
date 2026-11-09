// SPDX-License-Identifier: Apache-2.0
//
//! `hellohq-wasm-host`: runs ONE Wasm plugin session out of the app's process.
//!
//! The app starts this helper, sends a START frame on stdin, and answers the
//! events it writes on stdout (see [`hellohq_wasm_runtime::host_protocol`]).
//! Everything a plugin can do still goes through the app: host calls arrive as
//! PENDING frames and wasi:http / ai:inference traffic as OUT frames, exactly as
//! the in-process C ABI surfaces them. The helper itself needs no files and no
//! network, which is what lets it run under a far stricter sandbox than the app,
//! and lets the app stop a run at any moment by ending the process.
//!
//! Exit codes: 0 after DONE or ERROR was written; 2 when the app went away
//! (stdin closed) or broke the protocol; 3 when stdout could not be written.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::ExitCode;

use hellohq_wasm_runtime::host_protocol::{
    encode_ready, read_frame, write_frame, SessionKind, Start, FRAME_DONE, FRAME_ERROR, FRAME_OUT,
    FRAME_OUT_END, FRAME_PENDING, FRAME_PUSH, FRAME_PUSH_END, FRAME_READY, FRAME_RESOLVE,
    FRAME_START,
};
use hellohq_wasm_runtime::{
    hwr_abi_version, hwr_last_error, hwr_p3_free, hwr_p3_poll, hwr_p3_request_len,
    hwr_p3_request_ptr, hwr_p3_resolve, hwr_p3_result_len, hwr_p3_result_ptr, hwr_p3_start_plugin,
    hwr_p3_start_plugin_compile, hwr_p3s_free, hwr_p3s_out_len, hwr_p3s_out_ptr, hwr_p3s_poll,
    hwr_p3s_push, hwr_p3s_push_end, hwr_p3s_result_len, hwr_p3s_result_ptr, hwr_p3s_start_http,
    hwr_p3s_start_inference, hwr_set_fuel_budget, HWR_P3S_DONE, HWR_P3S_ERROR, HWR_P3S_OUT,
    HWR_P3S_OUT_END, HWR_P3_DONE, HWR_P3_ERROR, HWR_P3_PENDING,
};

/// Why the helper stopped early. Each maps to an exit code.
enum Stop {
    /// The app closed stdin or sent something out of turn.
    App(String),
    /// stdout failed: nobody is listening any more.
    Output(io::Error),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Output(e)
    }
}

fn main() -> ExitCode {
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    match run(&mut input, &mut output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Stop::App(why)) => {
            let _ = writeln!(io::stderr(), "hellohq-wasm-host: {why}");
            ExitCode::from(2)
        }
        Err(Stop::Output(e)) => {
            let _ = writeln!(io::stderr(), "hellohq-wasm-host: output failed: {e}");
            ExitCode::from(3)
        }
    }
}

fn run(input: &mut impl Read, output: &mut impl Write) -> Result<(), Stop> {
    write_frame(output, FRAME_READY, &encode_ready(hwr_abi_version()))?;

    let start = match next_frame(input)? {
        (FRAME_START, payload) => Start::decode(&payload).map_err(Stop::App)?,
        (ty, _) => return Err(Stop::App(format!("expected START, got frame {ty:#04x}"))),
    };
    if start.fuel > 0 {
        hwr_set_fuel_budget(start.fuel);
    }
    if start.kind.is_stream() {
        run_stream(&start, input, output)
    } else {
        run_session(&start, input, output)
    }
}

/// The next frame from the app; a closed stdin ends the run.
fn next_frame(input: &mut impl Read) -> Result<(u8, Vec<u8>), Stop> {
    match read_frame(input) {
        Ok(Some(frame)) => Ok(frame),
        Ok(None) => Err(Stop::App("the app closed the session".into())),
        Err(e) => Err(Stop::App(format!("unreadable frame: {e}"))),
    }
}

/// Why the start call just made on this thread failed: the runtime's
/// `hwr_last_error` line (a stable prefix, no plugin text), so the app can tell
/// a build without the JIT entitlement from a broken plugin (#30).
fn start_failure() -> String {
    let ptr = hwr_last_error();
    if ptr.is_null() {
        return "the plugin could not be started".to_string();
    }
    // SAFETY: a NUL-terminated string owned by the runtime, valid until the
    // next runtime call on this thread; copied here before any other call.
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Host-call sessions (`hwr_p3_*`): PENDING ↔ RESOLVE until DONE or ERROR.
fn run_session(start: &Start, input: &mut impl Read, output: &mut impl Write) -> Result<(), Stop> {
    let input_ptr = if start.input.is_empty() {
        std::ptr::null()
    } else {
        start.input.as_ptr()
    };
    // SAFETY: both buffers outlive the call; the runtime copies them.
    let session = unsafe {
        let f = match start.kind {
            SessionKind::PluginCompile => hwr_p3_start_plugin_compile,
            _ => hwr_p3_start_plugin,
        };
        f(
            start.use_pulley as i32,
            start.component.as_ptr(),
            start.component.len(),
            input_ptr,
            start.input.len(),
        )
    };
    if session.is_null() {
        write_frame(output, FRAME_ERROR, start_failure().as_bytes())?;
        return Ok(());
    }

    let result = (|| loop {
        // SAFETY: `session` is live until hwr_p3_free below.
        match unsafe { hwr_p3_poll(session) } {
            HWR_P3_PENDING => {
                let request = unsafe {
                    std::slice::from_raw_parts(
                        hwr_p3_request_ptr(session),
                        hwr_p3_request_len(session),
                    )
                };
                write_frame(output, FRAME_PENDING, request)?;
                match next_frame(input)? {
                    (FRAME_RESOLVE, value) => unsafe {
                        hwr_p3_resolve(session, value.as_ptr(), value.len())
                    },
                    (ty, _) => {
                        return Err(Stop::App(format!("expected RESOLVE, got frame {ty:#04x}")))
                    }
                }
            }
            status @ (HWR_P3_DONE | HWR_P3_ERROR) => {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        hwr_p3_result_ptr(session),
                        hwr_p3_result_len(session),
                    )
                };
                let ty = if status == HWR_P3_DONE {
                    FRAME_DONE
                } else {
                    FRAME_ERROR
                };
                write_frame(output, ty, bytes)?;
                return Ok(());
            }
            other => {
                write_frame(
                    output,
                    FRAME_ERROR,
                    format!("unexpected runtime status {other}").as_bytes(),
                )?;
                return Ok(());
            }
        }
    })();
    // SAFETY: freed exactly once; cancels and joins the run if still going.
    unsafe { hwr_p3_free(session) };
    result
}

/// Streaming sessions (`hwr_p3s_*`): OUT… OUT_END, then the app's PUSH…
/// PUSH_END, then DONE or ERROR. The app answers every OUT_END with a
/// PUSH_END (possibly with no PUSH before it), as the in-process loop does.
fn run_stream(start: &Start, input: &mut impl Read, output: &mut impl Write) -> Result<(), Stop> {
    // SAFETY: the component buffer outlives the call; the runtime copies it.
    let session = unsafe {
        let f = match start.kind {
            SessionKind::Http => hwr_p3s_start_http,
            _ => hwr_p3s_start_inference,
        };
        f(
            start.use_pulley as i32,
            start.component.as_ptr(),
            start.component.len(),
        )
    };
    if session.is_null() {
        write_frame(output, FRAME_ERROR, start_failure().as_bytes())?;
        return Ok(());
    }

    let result = (|| loop {
        // SAFETY: `session` is live until hwr_p3s_free below.
        match unsafe { hwr_p3s_poll(session) } {
            HWR_P3S_OUT => {
                let chunk = unsafe {
                    std::slice::from_raw_parts(hwr_p3s_out_ptr(session), hwr_p3s_out_len(session))
                };
                write_frame(output, FRAME_OUT, chunk)?;
            }
            HWR_P3S_OUT_END => {
                write_frame(output, FRAME_OUT_END, b"")?;
                loop {
                    match next_frame(input)? {
                        (FRAME_PUSH, chunk) => unsafe {
                            hwr_p3s_push(session, chunk.as_ptr(), chunk.len())
                        },
                        (FRAME_PUSH_END, _) => {
                            unsafe { hwr_p3s_push_end(session) };
                            break;
                        }
                        (ty, _) => {
                            return Err(Stop::App(format!(
                                "expected PUSH or PUSH_END, got frame {ty:#04x}"
                            )))
                        }
                    }
                }
            }
            status @ (HWR_P3S_DONE | HWR_P3S_ERROR) => {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        hwr_p3s_result_ptr(session),
                        hwr_p3s_result_len(session),
                    )
                };
                let ty = if status == HWR_P3S_DONE {
                    FRAME_DONE
                } else {
                    FRAME_ERROR
                };
                write_frame(output, ty, bytes)?;
                return Ok(());
            }
            other => {
                write_frame(
                    output,
                    FRAME_ERROR,
                    format!("unexpected runtime status {other}").as_bytes(),
                )?;
                return Ok(());
            }
        }
    })();
    // SAFETY: freed exactly once; closes inbound and joins the run.
    unsafe { hwr_p3s_free(session) };
    result
}
