#!/bin/sh
# This host's Homebrew rustc bypasses rustup; use the installed project toolchain.
set -eu
cd "$(dirname "$0")/.."
launcher_toolchain_bin=$(dirname "$(rustup which --toolchain 1.96.0 rustc)")
export PATH="$launcher_toolchain_bin:$PATH"
export RUSTC="$launcher_toolchain_bin/rustc"
export RUSTDOC="$launcher_toolchain_bin/rustdoc"
exec rustup run 1.96.0 cargo "$@"
