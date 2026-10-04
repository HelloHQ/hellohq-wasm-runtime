/* SPDX-License-Identifier: Apache-2.0
 *
 * C ABI for hellohq-wasm-runtime — consumed by dart:ffi (hellohq app) and by the
 * iOS XCTest latency harness (ios-bench/). Hand-maintained to mirror the
 * #[no_mangle] extern "C" surface in src/lib.rs; bump HWR_ABI_VERSION there and
 * here together.
 *
 * Availability: functions marked [no-JIT] are present in every build, including
 * the iOS Pulley build (`--no-default-features`). Functions marked [compile]
 * require Cranelift + the `wat` parser and exist only in desktop/Android/CI
 * builds — they are NOT linked into the iOS slice, so the device harness must
 * use the precompiled deserialize path (hwr_instance_new_precompiled). A
 * feature name in the marker ([wasi-http], [typed-hosts]) means the function
 * also needs that cargo feature; the release libraries (scripts/
 * build-release.sh) are built with `wasi-http typed-hosts` on every platform.
 */
#ifndef HELLOHQ_WASM_RUNTIME_H
#define HELLOHQ_WASM_RUNTIME_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Error sentinel returned by the i64 call ABI (== INT64_MIN). */
#define HWR_CALL_ERROR INT64_MIN

/* Opaque handles. */
typedef struct HwrEngine HwrEngine;
typedef struct HwrInstance HwrInstance;

/* ── Handshake / smoke test ──────────────────────────────────────────────── */
#define HWR_ABI_VERSION 2 /* == hwr_abi_version(); 2 = tagged http OUT frames */
uint32_t hwr_abi_version(void);          /* [no-JIT] */
int32_t  hwr_self_test(void);            /* [no-JIT] 1 = runtime links + inits  */

/* ── Engine + instance lifecycle (the iOS execution path) ────────────────── */
HwrEngine*   hwr_engine_new(int32_t use_pulley);                 /* [no-JIT] */
void         hwr_engine_free(HwrEngine* engine);                 /* [no-JIT] */
void         hwr_instance_free(HwrInstance* instance);           /* [no-JIT] */
int64_t      hwr_instance_call_add(HwrInstance* instance,
                                   int32_t a, int32_t b);        /* [no-JIT] */

/* Deserialize a precompiled (AOT) component artifact and instantiate it. This
 * is the iOS model: artifacts are produced off-device by hwr_precompile_component
 * (or Component::serialize) under Cranelift, shipped, and run here with no JIT. */
HwrInstance* hwr_instance_new_precompiled(HwrEngine* engine,
                                          const uint8_t* bytes,
                                          size_t len);           /* [no-JIT] */
void         hwr_free_bytes(uint8_t* ptr, size_t len);           /* [no-JIT] */

/* ── P3: Dart-serviced host-call round-trip (step/poll bridge) ───────────── */
/* A host import the guest calls suspends the run; the request surfaces via the
 * blocking hwr_p3_poll; the caller services it (gated, app-side) and resolves
 * the value; the run resumes. wasi:http + ai:inference ride this round-trip. */
typedef struct HwrP3Session HwrP3Session;
#define HWR_P3_PENDING 1 /* host call awaits a value: read request, then resolve */
#define HWR_P3_DONE    2 /* run finished OK: read result                        */
#define HWR_P3_ERROR   3 /* run errored: result holds a UTF-8 message           */

/* Start a run from a PRECOMPILED component (deserialize) — the iOS path. */
HwrP3Session* hwr_p3_start(int32_t use_pulley, const uint8_t* component, size_t component_len,
                           const uint8_t* input, size_t input_len);          /* [no-JIT] */
/* C1 typed-capability runs: the guest imports the typed hellohq:plugin
 * interfaces; each typed call surfaces as a JSON {"method":…} host call on the
 * same poll/resolve round-trip. Each comes as a DESERIALIZE variant (precompiled
 * artifact; the iOS path) and a _compile variant (raw component; Cranelift).
 *   workspace       — imports hellohq:plugin/workspace, exports run() -> list<u8>
 *   storage_events  — imports hellohq:plugin/{storage,events}, same export
 *   plugin          — a real SDK plugin: imports hellohq:plugin/{workspace,
 *                     storage,events,log}, exports the canonical `guest`
 *                     interface; drives guest.run(input). The production
 *                     entrypoint the app binds. */
HwrP3Session* hwr_p3_start_workspace(int32_t use_pulley, const uint8_t* component,
                                     size_t component_len);            /* [no-JIT,typed-hosts] */
HwrP3Session* hwr_p3_start_storage_events(int32_t use_pulley, const uint8_t* component,
                                          size_t component_len);       /* [no-JIT,typed-hosts] */
HwrP3Session* hwr_p3_start_plugin(int32_t use_pulley, const uint8_t* component,
                                  size_t component_len,
                                  const uint8_t* input, size_t input_len); /* [no-JIT,typed-hosts] */
int32_t        hwr_p3_poll(HwrP3Session*);            /* [no-JIT] BLOCKS; HWR_P3_*       */
const uint8_t* hwr_p3_request_ptr(HwrP3Session*);     /* [no-JIT] valid until resolve    */
size_t         hwr_p3_request_len(HwrP3Session*);     /* [no-JIT] */
void           hwr_p3_resolve(HwrP3Session*, const uint8_t* response, size_t response_len); /* [no-JIT] */
const uint8_t* hwr_p3_result_ptr(HwrP3Session*);      /* [no-JIT] valid after DONE/ERROR */
size_t         hwr_p3_result_len(HwrP3Session*);      /* [no-JIT] */
void           hwr_p3_free(HwrP3Session*);            /* [no-JIT] cancels + joins        */

/* ── P3 v2: streaming host-call round-trip (framed bidirectional channel) ── */
/* For STREAMED bodies (wasi:http): the request body flows OUT (host -> caller)
 * chunk by chunk, the response body flows IN (caller -> host). The caller drains
 * OUT/OUT_END, then pushes IN chunks + push_end, then polls for DONE. */
typedef struct HwrP3Stream HwrP3Stream;
#define HWR_P3S_OUT      0 /* an outbound chunk is ready (read via out ptr/len)  */
#define HWR_P3S_OUT_END  1 /* outbound (request) finished; now push inbound      */
#define HWR_P3S_DONE     2 /* run finished OK (read result)                      */
#define HWR_P3S_ERROR    3 /* run errored (result holds a UTF-8 message)         */

int32_t        hwr_p3s_poll(HwrP3Stream*);             /* [no-JIT] BLOCKS; HWR_P3S_*      */
const uint8_t* hwr_p3s_out_ptr(HwrP3Stream*);          /* [no-JIT] current outbound chunk */
size_t         hwr_p3s_out_len(HwrP3Stream*);          /* [no-JIT] */
void           hwr_p3s_push(HwrP3Stream*, const uint8_t* chunk, size_t len); /* [no-JIT] inbound chunk */
void           hwr_p3s_push_end(HwrP3Stream*);         /* [no-JIT] close inbound          */
const uint8_t* hwr_p3s_result_ptr(HwrP3Stream*);       /* [no-JIT] after DONE/ERROR       */
size_t         hwr_p3s_result_len(HwrP3Stream*);       /* [no-JIT] */
void           hwr_p3s_free(HwrP3Stream*);             /* [no-JIT] closes inbound + joins */

/* ── Compile-time only (Cranelift); NOT in the iOS slice ─────────────────── */
/* Run the wasi:http guest [component], routing handler.handle through the P3 v2
 * transport: the guest's outbound request surfaces as OUT frames, the caller
 * (Dart) services it (gated) and pushes the response IN. (wasi-http feature.)
 *
 * http OUT frames (src/wasi_http_frames.rs) — every frame is [kind:u8][payload],
 * in the order HEAD BODY* TRAILERS?, then OUT_END:
 *   HWR_HTTP_OUT_HEAD      "{METHOD} {scheme}://{authority}{path}" then
 *                          "\n{name}: {value}" per header, then at most one
 *                          "\nx-hellohq-request-options: connect=<ns>;…" line
 *   HWR_HTTP_OUT_BODY      raw request-body bytes (never empty)
 *   HWR_HTTP_OUT_TRAILERS  "{name}=<hex(value)>;…" request trailer fields
 * Dispatch on the kind byte only. Runtimes <= v0.0.2 sent untagged frames (the
 * head began with the method name). IN frames are unchanged: frame 1 is the
 * response head "{status}\n{name}: {value}…" (optionally an
 * "x-hellohq-trailers: {name}=<hex>;…" line), frames 2..N the body. */
#define HWR_HTTP_OUT_HEAD     0x01
#define HWR_HTTP_OUT_BODY     0x02
#define HWR_HTTP_OUT_TRAILERS 0x03
HwrP3Stream* hwr_p3s_start_http(int32_t use_pulley, const uint8_t* component,
                                size_t component_len);                          /* [compile,wasi-http] */
/* Run the hellohq:plugin/inference guest by COMPILING [component] (Cranelift). */
HwrP3Stream* hwr_p3s_start_inference(int32_t use_pulley, const uint8_t* component,
                                     size_t component_len);                     /* [compile,wasi-http] */

/* ── No-JIT streaming (iOS): DESERIALIZE a precompiled pulley64 artifact ──── */
/* No-Cranelift twins of the above: deserialize a precompiled (pulley64) artifact
 * (from hwr_precompile_component) and run it over the P3 v2 transport. These ship
 * in the iOS no-JIT slice (wasi-http feature only). */
HwrP3Stream* hwr_p3s_start_http_precompiled(int32_t use_pulley, const uint8_t* component,
                                            size_t component_len);              /* [wasi-http] */
HwrP3Stream* hwr_p3s_start_inference_precompiled(int32_t use_pulley, const uint8_t* component,
                                                 size_t component_len);         /* [wasi-http] */

/* Start a P3 run by COMPILING a raw component (desktop/Android; host tests). */
HwrP3Session* hwr_p3_start_compile(int32_t use_pulley, const uint8_t* component, size_t component_len,
                                   const uint8_t* input, size_t input_len);  /* [compile] */
/* COMPILE variants of the typed-capability runs above. */
HwrP3Session* hwr_p3_start_workspace_compile(int32_t use_pulley, const uint8_t* component,
                                             size_t component_len);      /* [compile,typed-hosts] */
HwrP3Session* hwr_p3_start_storage_events_compile(int32_t use_pulley, const uint8_t* component,
                                                  size_t component_len); /* [compile,typed-hosts] */
HwrP3Session* hwr_p3_start_plugin_compile(int32_t use_pulley, const uint8_t* component,
                                          size_t component_len,
                                          const uint8_t* input, size_t input_len); /* [compile,typed-hosts] */
/* P2 smoke test: the gated workspace.read-portfolio-names component. granted != 0
 * returns `count`; denied returns UINT32_MAX (as i64); INT64_MIN on error. */
int64_t      hwr_read_portfolio_count(int32_t use_pulley, int32_t granted,
                                      uint32_t count);                     /* [compile] */
int64_t      hwr_eval_add(int32_t use_pulley, int32_t a, int32_t b);            /* [compile] */
int64_t      hwr_eval_component_add(int32_t use_pulley, int32_t a, int32_t b);  /* [compile] */
int64_t      hwr_eval_host_import(int32_t use_pulley, int32_t x);               /* [compile] */
int64_t      hwr_run_async_double(int32_t use_pulley, int32_t x);               /* [compile] */
int64_t      hwr_run_component_async_double(int32_t use_pulley, int32_t x);     /* [compile] */
int64_t      hwr_run_canonical_async_double(int32_t use_pulley, int32_t x);     /* [compile] */
HwrInstance* hwr_instance_new(HwrEngine* engine,
                              const uint8_t* wasm, size_t len);                 /* [compile] */
uint8_t*     hwr_precompile_component(HwrEngine* engine,
                                      const uint8_t* wasm, size_t len,
                                      size_t* out_len);                         /* [compile] */

#ifdef __cplusplus
}
#endif

#endif /* HELLOHQ_WASM_RUNTIME_H */
