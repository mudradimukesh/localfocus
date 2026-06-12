#!/usr/bin/env bash
# Cross-compile the Local Focus Rust core (the same binary the Mac app uses) for
# every Android ABI and drop each one into the Flutter app's jniLibs as a
# native library. The Android app extracts these and execs them as the on-device
# Local Focus server (`serve`), so the phone runs the identical core + dashboard.
#
# Requirements: rustup, an installed Android NDK, and the Android Rust targets:
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JNI_LIBS="$ROOT/mobile/local_focus_mobile/android/app/src/main/jniLibs"
LIB_NAME="liblocalfocus.so"
API=24

# Locate the NDK (env override wins, else newest under the SDK).
NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK:-}}"
if [ -z "$NDK" ]; then
  NDK=$(ls -d "$HOME"/Library/Android/sdk/ndk/*/ 2>/dev/null | sort -V | tail -1 || true)
fi
[ -n "$NDK" ] || { echo "ERROR: Android NDK not found. Set ANDROID_NDK_HOME."; exit 1; }
NDK="${NDK%/}"
PREBUILT=$(ls -d "$NDK"/toolchains/llvm/prebuilt/*/ | head -1)
BIN="${PREBUILT}bin"
echo "Using NDK: $NDK"

# triple : android-abi-dir : clang-prefix
TARGETS=(
  "aarch64-linux-android:arm64-v8a:aarch64-linux-android"
  "armv7-linux-androideabi:armeabi-v7a:armv7a-linux-androideabi"
  "x86_64-linux-android:x86_64:x86_64-linux-android"
  "i686-linux-android:x86:i686-linux-android"
)

for entry in "${TARGETS[@]}"; do
  IFS=":" read -r triple abi clang <<<"$entry"
  linker="$BIN/${clang}${API}-clang"
  [ -x "$linker" ] || { echo "skip $abi (no linker $linker)"; continue; }

  # The core is pure Rust (no C deps), so only the linker is needed.
  upper=$(echo "$triple" | tr 'a-z-' 'A-Z_')
  export "CARGO_TARGET_${upper}_LINKER=$linker"

  echo "==> building $triple ($abi)"
  ( cd "$ROOT" && cargo build --release --target "$triple" )

  mkdir -p "$JNI_LIBS/$abi"
  cp "$ROOT/target/$triple/release/local-focus" "$JNI_LIBS/$abi/$LIB_NAME"
  echo "    -> $JNI_LIBS/$abi/$LIB_NAME"
done

echo "Done. Packaged ABIs:"
ls -1 "$JNI_LIBS"
