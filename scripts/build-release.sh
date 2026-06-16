#!/usr/bin/env bash
# Build the HelloHQ Wasm runtime native libs for the CURRENT host's platforms
# and stage them (+ SHA256SUMS) for the release pipeline
# (.github/workflows/release.yml). P1 of the WASI-0.3 migration (docs 51/52).
#
# Feature matrix (KEY: the JIT compiler ships only where the platform allows it):
#   Desktop / Android → DEFAULT features  → Cranelift JIT  (cdylib .so/.dylib/.dll)
#   iOS               → --no-default-features (runtime + Pulley interpreter, NO
#                        Cranelift; modules are precompiled off-device) → staticlib
#                        assembled into an xcframework.
#
#   macOS  → libhellohq_wasm_runtime.dylib (per-arch) + HelloHQWasmRuntime.xcframework (iOS)
#   Linux  → libhellohq_wasm_runtime.so (x86_64) [+ Android jniLibs when cargo-ndk present]
#   Windows→ hellohq_wasm_runtime.dll (x86_64)
#
# Release assets are one zip per platform-dir so basenames stay unique when the
# CI matrix flattens every runner's dist/ into a single release (the two macOS
# dylibs share a filename but differ by arch, hence per-platform zips rather
# than a flat copy). The app-side fetch script unzips the asset for the running
# platform into third_party/wasm_runtime/<platformDir>/. Assets produced:
#   dist/macos-arm64.zip   → macos-arm64/libhellohq_wasm_runtime.dylib
#   dist/macos-x64.zip     → macos-x64/libhellohq_wasm_runtime.dylib
#   dist/linux-x64.zip     → linux-x64/libhellohq_wasm_runtime.so
#   dist/windows-x64.zip   → windows-x64/hellohq_wasm_runtime.dll
#   dist/jniLibs.zip       → jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/libhellohq_wasm_runtime.so
#   dist/HelloHQWasmRuntime.xcframework.zip
# The <platformDir> names match hellohq's loader (wasm_runtime_loader.dart
# _platformDir()): macos-arm64, macos-x64, linux-x64, windows-x64.
#
# Each platform is guarded behind a detected-toolchain check so this runs in a
# CI matrix (one OS per runner) — missing targets/SDKs are skipped with a log
# line rather than failing the job.
set -euo pipefail

CRATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAGE="${1:-$CRATE_DIR/dist}"
cd "$CRATE_DIR"
rm -rf "$STAGE" && mkdir -p "$STAGE"

# cdylib basenames produced by cargo per platform (lib name = hellohq_wasm_runtime).
DYLIB=libhellohq_wasm_runtime.dylib
SO=libhellohq_wasm_runtime.so
DLL=hellohq_wasm_runtime.dll
STATICLIB=libhellohq_wasm_runtime.a

# True if the given rustup target is installed (toolchain present for it).
has_target() { rustup target list --installed 2>/dev/null | grep -qx "$1"; }

# Zip a staged platform dir into <dir>.zip (preserving the dir prefix) so the
# release asset basename is unique, then drop the unzipped dir.
zip_platform_dir() {
  ( cd "$STAGE"
    # GitHub's Windows runners' Git-Bash has no `zip`; fall back to 7z (present on
    # the runner) or PowerShell Compress-Archive. Linux/macOS use `zip`.
    if command -v zip >/dev/null 2>&1; then
      zip -qry "$1.zip" "$1"
    elif command -v 7z >/dev/null 2>&1; then
      7z a -tzip -bso0 -bsp0 "$1.zip" "$1" >/dev/null
    else
      powershell -NoProfile -Command "Compress-Archive -Path '$1' -DestinationPath '$1.zip' -Force"
    fi
    rm -rf "$1" )
}

# Desktop / Android: DEFAULT features → Cranelift JIT.
build_jit() {
  echo "→ cargo build --release --target $1  (default features / Cranelift)"
  cargo build --release --target "$1"
}

# iOS: drop Cranelift. runtime + Pulley + component-model + async + std only.
build_nojit() {
  echo "→ cargo build --release --no-default-features --target $1  (Pulley, no JIT)"
  cargo build --release --no-default-features --target "$1"
}

# ---------------------------------------------------------------------------
# Android: cdylib .so into jniLibs ABI dirs (arm64-v8a, armeabi-v7a, x86_64).
# Uses cargo-ndk, which keys off ANDROID_NDK_HOME for the toolchain/linker.
# Default features → Cranelift JIT (allowed on Android).
# ---------------------------------------------------------------------------
build_android() {
  if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "⚠ cargo-ndk not found; skipping Android (install: cargo install cargo-ndk)"; return 0
  fi
  if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    echo "⚠ ANDROID_NDK_HOME unset; skipping Android"; return 0
  fi
  echo "→ cargo ndk → jniLibs (arm64-v8a, armeabi-v7a, x86_64)  (default features / Cranelift)"
  cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o "$STAGE/jniLibs" build --release
  ( cd "$STAGE" && zip -qry jniLibs.zip jniLibs && rm -rf jniLibs )
}

# ---------------------------------------------------------------------------
# iOS: assemble an xcframework from the no-JIT STATICLIBS.
#   device     = aarch64-apple-ios
#   simulator  = aarch64-apple-ios-sim + x86_64-apple-ios-sim (lipo'd fat)
# A static xcframework keeps the < 8 MB Spike-1 budget (Pulley, no Cranelift)
# and lets Xcode link the FFI symbols directly into the app binary.
# ---------------------------------------------------------------------------
build_ios() {
  local dev_t=aarch64-apple-ios sim_arm=aarch64-apple-ios-sim sim_x64=x86_64-apple-ios-sim
  if ! command -v xcodebuild >/dev/null 2>&1; then
    echo "⚠ xcodebuild not found; skipping iOS xcframework"; return 0
  fi
  if ! has_target "$dev_t"; then
    echo "⚠ $dev_t target not installed; skipping iOS (rustup target add $dev_t $sim_arm $sim_x64)"; return 0
  fi

  build_nojit "$dev_t"
  local dev_lib="target/$dev_t/release/$STATICLIB"

  # Simulator slice: fat lib of whatever sim arches are installed.
  local sim_inputs=()
  has_target "$sim_arm" && { build_nojit "$sim_arm"; sim_inputs+=("target/$sim_arm/release/$STATICLIB"); }
  has_target "$sim_x64" && { build_nojit "$sim_x64"; sim_inputs+=("target/$sim_x64/release/$STATICLIB"); }

  local dev_dir="$STAGE/_iosdev" sim_dir="$STAGE/_iossim"
  mkdir -p "$dev_dir" "$sim_dir"
  cp "$dev_lib" "$dev_dir/$STATICLIB"

  # Header dir bundled into every slice's Headers/ so the resulting xcframework
  # is importable as a Clang module (`import HelloHQWasmRuntime` from Swift).
  # Contains the hand-maintained C ABI header + its module.modulemap.
  local hdr_dir="$CRATE_DIR/include"

  local xc_args=(-library "$dev_dir/$STATICLIB" -headers "$hdr_dir")
  if [ "${#sim_inputs[@]}" -gt 0 ]; then
    lipo -create "${sim_inputs[@]}" -output "$sim_dir/$STATICLIB"
    xc_args+=(-library "$sim_dir/$STATICLIB" -headers "$hdr_dir")
  else
    echo "⚠ no iOS simulator targets installed; xcframework will contain device slice only"
  fi

  rm -rf "$STAGE/HelloHQWasmRuntime.xcframework"
  xcodebuild -create-xcframework "${xc_args[@]}" \
    -output "$STAGE/HelloHQWasmRuntime.xcframework"
  rm -rf "$dev_dir" "$sim_dir"
  ( cd "$STAGE" && zip -qry HelloHQWasmRuntime.xcframework.zip HelloHQWasmRuntime.xcframework \
      && rm -rf HelloHQWasmRuntime.xcframework )
}

# ---------------------------------------------------------------------------
# Per-host build matrix.
# ---------------------------------------------------------------------------
case "$(uname -s)" in
  Darwin)
    # Desktop dylib — build whichever apple-darwin arch this host has installed.
    for arch in arm64 x64; do
      case "$arch" in
        arm64) t=aarch64-apple-darwin   d=macos-arm64 ;;
        x64)   t=x86_64-apple-darwin    d=macos-x64   ;;
      esac
      if has_target "$t"; then
        build_jit "$t"
        mkdir -p "$STAGE/$d"
        cp "target/$t/release/$DYLIB" "$STAGE/$d/$DYLIB"
        install_name_tool -id "@rpath/$DYLIB" "$STAGE/$d/$DYLIB"
        # WASM_RUNTIME_NO_ZIP=1 leaves the dir unzipped (local inspection/test);
        # CI leaves it unset so each platform dir becomes a unique-named zip asset.
        [ -n "${WASM_RUNTIME_NO_ZIP:-}" ] || zip_platform_dir "$d"
      else
        echo "⚠ $t not installed; skipping $d desktop dylib"
      fi
    done
    build_ios
    build_android
    ;;
  Linux)
    t=x86_64-unknown-linux-gnu
    if has_target "$t"; then
      build_jit "$t"
      mkdir -p "$STAGE/linux-x64"
      cp "target/$t/release/$SO" "$STAGE/linux-x64/$SO"
      [ -n "${WASM_RUNTIME_NO_ZIP:-}" ] || zip_platform_dir linux-x64
    else
      echo "⚠ $t not installed; skipping linux-x64"
    fi
    build_android
    ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    t=x86_64-pc-windows-msvc
    if has_target "$t"; then
      build_jit "$t"
      mkdir -p "$STAGE/windows-x64"
      cp "target/$t/release/$DLL" "$STAGE/windows-x64/$DLL"
      [ -n "${WASM_RUNTIME_NO_ZIP:-}" ] || zip_platform_dir windows-x64
    else
      echo "⚠ $t not installed; skipping windows-x64"
    fi
    ;;
  *)
    echo "⚠ unsupported host $(uname -s); nothing built" ;;
esac

# SHA256SUMS over every staged file (relative paths), portable shasum/sha256sum.
( cd "$STAGE"
  files="$(find . -type f ! -name SHA256SUMS | sed 's#^\./##' | sort)"
  if [ -n "$files" ]; then
    # shellcheck disable=SC2086
    if command -v sha256sum >/dev/null 2>&1; then sha256sum $files > SHA256SUMS
    else shasum -a 256 $files > SHA256SUMS; fi
  fi
)
echo "Staged for $(uname -s):"
( cd "$STAGE" && find . -type f | sed 's#^\./##' | sort )
