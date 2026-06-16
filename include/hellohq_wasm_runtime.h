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
 * use the precompiled deserialize path (hwr_instance_new_precompiled).
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
 * (Dart) services it (gated) and pushes the response IN. (wasi-http feature.) */
HwrP3Stream* hwr_p3s_start_http(int32_t use_pulley, const uint8_t* component,
                                size_t component_len);                          /* [compile,wasi-http] */
/* Start a P3 run by COMPILING a raw component (desktop/Android; host tests). */
HwrP3Session* hwr_p3_start_compile(int32_t use_pulley, const uint8_t* component, size_t component_len,
                                   const uint8_t* input, size_t input_len);  /* [compile] */
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
