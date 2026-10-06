#!/bin/sh
# Builds libsharekvm_android.so for each Android CPU type into the app's jniLibs.
# Usage: android/build-rust.sh [release|debug]
set -e
PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* | sort -V | tail -1)}"
HOST=$(ls "$NDK/toolchains/llvm/prebuilt" | head -1)
BIN="$NDK/toolchains/llvm/prebuilt/$HOST/bin"
API=24 # minSdk

FLAG=""; [ "$PROFILE" = release ] && FLAG="--release"
for pair in "aarch64-linux-android:arm64-v8a:aarch64-linux-android" \
            "armv7-linux-androideabi:armeabi-v7a:armv7a-linux-androideabi" \
            "x86_64-linux-android:x86_64:x86_64-linux-android"; do
  TARGET=${pair%%:*}; rest=${pair#*:}; ABI=${rest%%:*}; CLANG=${rest#*:}
  ENVTARGET=$(echo "$TARGET" | tr 'a-z-' 'A-Z_')
  export "CARGO_TARGET_${ENVTARGET}_LINKER=$BIN/${CLANG}${API}-clang"
  export "CC_$(echo "$TARGET" | tr '-' '_')=$BIN/${CLANG}${API}-clang"
  export "AR_$(echo "$TARGET" | tr '-' '_')=$BIN/llvm-ar"
  echo "==> $ABI"
  cargo build -p sharekvm-android --target "$TARGET" $FLAG --manifest-path "$ROOT/Cargo.toml"
  mkdir -p "$ROOT/android/app/src/main/jniLibs/$ABI"
  cp "$ROOT/target/$TARGET/$PROFILE/libsharekvm_android.so" "$ROOT/android/app/src/main/jniLibs/$ABI/"
done
