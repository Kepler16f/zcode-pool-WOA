export PATH="/c/Users/胡天泽/AppData/Local/Android/Sdk/ndk/27.2.12479018/toolchains/llvm/prebuilt/windows-x86_64/bin:/c/Program Files/LLVM/bin:$PATH"
export CC_aarch64_linux_android="aarch64-linux-android24-clang.cmd"
export CXX_aarch64_linux_android="aarch64-linux-android24-clang++.cmd"
export AR_aarch64_linux_android="llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="aarch64-linux-android24-clang.cmd"
cd /d/zcode-pool/src-tauri
cargo check --target aarch64-linux-android -p zcode-pool 2>&1 | grep -E "^error" -A8 > /tmp/android-errors.txt
grep -cE "^error" /tmp/android-errors.txt
head -80 /tmp/android-errors.txt
