#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
target=${1:-x86_64-unknown-linux-musl}
case "$target" in
  x86_64-unknown-linux-musl) arch=x86_64 ;;
  aarch64-unknown-linux-musl) arch=aarch64 ;;
  *) echo "Usage: $0 [x86_64-unknown-linux-musl|aarch64-unknown-linux-musl]" >&2; exit 2 ;;
esac

version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)
host=$(rustc -vV | sed -n 's/^host: //p')
sysroot=$(rustc --print sysroot)
linker="$sysroot/lib/rustlib/$host/bin/rust-lld"
if [[ ! -x "$linker" ]]; then
  echo "Rust's bundled linker is missing: $linker" >&2
  exit 1
fi
rustup target add "$target"
target_key=${target^^}
target_key=${target_key//-/_}
export "CARGO_TARGET_${target_key}_LINKER=$linker"
# Baseline CPU code allows hashing implementations to select acceleration at runtime.
unset CARGO_ENCODED_RUSTFLAGS
export RUSTFLAGS='-C target-feature=+crt-static -C link-self-contained=yes'

# Set CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 when cross-testing.
cargo test --locked --target "$target" --all-targets
cargo build --locked --release --target "$target"
binary="${CARGO_TARGET_DIR:-target}/$target/release/rustide"
program_headers=$(readelf -l "$binary")
dynamic_section=$(readelf -d "$binary")
if grep -q 'INTERP' <<< "$program_headers"; then
  echo "Release unexpectedly needs a dynamic loader" >&2
  exit 1
fi
if grep -q '(NEEDED)' <<< "$dynamic_section"; then
  echo "Release unexpectedly needs shared libraries" >&2
  exit 1
fi

mkdir -p dist
package="rustide-v${version}-linux-${arch}"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$package"
install -m 755 "$binary" "$stage/$package/rustide"
install -m 644 LICENSE "$stage/$package/LICENSE"
install -m 644 THIRD_PARTY_LICENSES.txt "$stage/$package/THIRD_PARTY_LICENSES.txt"
if [[ -f README.md ]]; then
  install -m 644 README.md "$stage/$package/README.md"
fi
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
  -C "$stage" -cf - "$package" | gzip -n > "dist/$package.tar.gz"
(cd dist && sha256sum "$package.tar.gz" > "$package.tar.gz.sha256")
echo "Built dist/$package.tar.gz"
