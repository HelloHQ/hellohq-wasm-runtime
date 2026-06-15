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

/* ── Compile-time only (Cranelift); NOT in the iOS slice ─────────────────── */
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
