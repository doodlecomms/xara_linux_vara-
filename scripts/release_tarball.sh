#!/bin/bash
set -euo pipefail
ref=${1:-HEAD}
root=$(git rev-parse --show-toplevel)
out=$(mkdir -p "${2:-$root/target/dist}" && cd "${2:-$root/target/dist}" && pwd)
ver=$(git -C "$root" show "$ref:Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p;T;q')
name=xara-$ver
mtime=$(git -C "$root" log -1 --format=%ct "$ref")
stage=$out/stage
rm -rf "$stage" && mkdir -p "$stage"
git -C "$root" archive --format=tar --prefix="$name/" "$ref" -- . ':!.gitignore' | xz -9 > "$out/$name.tar.xz"
xz -dc "$out/$name.tar.xz" | tar -x -C "$stage"
mkdir -p "$stage/$name/.cargo"
(cd "$stage/$name" && cargo vendor --offline --locked vendor > .cargo/config.toml)
find "$stage/$name/vendor" -path '*-pc-windows-gnu*/lib/*.a' -delete
python3 -I -c '
import json, os, sys
for c in os.scandir(sys.argv[1]):
    p = os.path.join(c.path, ".cargo-checksum.json")
    j = json.load(open(p))
    j["files"] = {f: h for f, h in j["files"].items() if os.path.exists(os.path.join(c.path, f))}
    json.dump(j, open(p, "w"))
' "$stage/$name/vendor"
tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" \
    --mode=u+rwX,go+rX,go-w \
    -cf - "$name/vendor" "$name/.cargo" | xz -9 > "$out/$name-vendor.tar.xz"
rm -rf "$stage"
"$root/scripts/release_buildinfo.sh" > "$out/$name.buildinfo"
ls -l "$out/$name.tar.xz" "$out/$name-vendor.tar.xz" "$out/$name.buildinfo"
