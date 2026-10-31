#!/bin/bash
set -euo pipefail
registered=
if [ "${1:-}" = --registered ]; then
    registered=1
    shift
fi
target=$1
root=$(cd "$(dirname "$0")/.." && pwd)
out=$(mkdir -p "${2:-$root/target/dist}" && cd "${2:-$root/target/dist}" && pwd)
ver=$(sed -n 's/^version = "\(.*\)"$/\1/p;T;q' "$root/Cargo.toml")
name=xara-$ver-$target
mtime=$(git -C "$root" log -1 --format=%ct)
case $target in
aarch64-unknown-linux-gnu) cc=aarch64-linux-gnu ;;
armv7-unknown-linux-gnueabihf) cc=arm-linux-gnueabihf ;;
*) cc= ;;
esac
if [ -n "$cc" ] && [ "$(uname -m)" = x86_64 ]; then
    t=$(echo "$target" | tr a-z- A-Z_)
    export "CARGO_TARGET_${t}_LINKER=$cc-gcc"
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_LIBDIR=/usr/lib/$cc/pkgconfig:/usr/share/pkgconfig
fi
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
if [ -n "$registered" ]; then
    dir=$root/target/registered
    trap 'rm -rf "$dir"' EXIT
    CARGO_TARGET_DIR=$dir cargo build --manifest-path "$root/Cargo.toml" --locked --release \
        --target "$target" -p xara --features registered >&2
    (cd "$dir/$target/release" && sha256sum xara) | sed "s|  xara\$|  registered/$name/xara|"
    exit
fi
cargo build --manifest-path "$root/Cargo.toml" --locked --release --target "$target" -p xara
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$root/target/$target/release/xara" "$root/target/$target/release/xara-monitor" \
    "$root/LICENSE" "$root/README.md" "$stage/$name/"
cp -r "$root/dist" "$stage/$name/"
tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@$mtime" --mode=u+rwX,go+rX,go-w -cf - "$name" | xz -9 > "$out/$name.tar.xz"
"$root/scripts/release_buildinfo.sh" > "$out/$name.buildinfo"
ls -l "$out/$name.tar.xz" "$out/$name.buildinfo"
