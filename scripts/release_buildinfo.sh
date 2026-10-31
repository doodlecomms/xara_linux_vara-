#!/bin/bash
set -euo pipefail
rustc -vV
cargo -V
if command -v dpkg-query >/dev/null; then
    dpkg-query -W -f='${db:Status-Abbrev} ${Package}:${Architecture} ${Version}\n' |
        awk '$1 == "ii" { print $2, $3 }' | LC_ALL=C sort
fi
