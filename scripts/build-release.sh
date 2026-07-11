#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
host=$(rustc -vV | sed -n 's/^host: //p')
build_target=${BUILD_TARGET:-$host}
target_dir=${CARGO_TARGET_DIR:-target}
binary_name=latte-lens
binary_suffix=""

if [[ -n "${BUILD_TARGET:-}" ]]; then
  binary_path="$target_dir/$BUILD_TARGET/release/$binary_name"
else
  binary_path="$target_dir/release/$binary_name"
fi

if [[ "$build_target" == *windows* ]]; then
  binary_suffix=".exe"
  binary_path+="$binary_suffix"
fi

if [[ -n "${BUILD_TARGET:-}" ]]; then
  cargo build --release --locked --target "$BUILD_TARGET"
else
  cargo build --release --locked
fi

package_name="latte-lens-${version}-${build_target}"
package_dir="dist/$package_name"
archive="dist/$package_name.tar.gz"
checksum="$archive.sha256"

rm -rf "$package_dir"
rm -f "$archive" "$checksum"
mkdir -p "$package_dir"
cp "$binary_path" "$package_dir/$binary_name$binary_suffix"
cp README.md LICENSE "$package_dir/"
COPYFILE_DISABLE=1 tar -C dist -czf "$archive" "$package_name"
rm -rf "$package_dir"

archive_name=$(basename "$archive")
if command -v sha256sum >/dev/null 2>&1; then
  (cd dist && sha256sum "$archive_name") > "$checksum"
else
  (cd dist && shasum -a 256 "$archive_name") > "$checksum"
fi

echo "Created $archive"
echo "Created $checksum"
