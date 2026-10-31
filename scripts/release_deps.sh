#!/bin/bash
set -euo pipefail
target=$1
pkgs=(ca-certificates curl gcc git libc6-dev pkg-config python3 xz-utils)
case $target in
aarch64-unknown-linux-gnu) arch=arm64 cc=aarch64-linux-gnu ;;
armv7-unknown-linux-gnueabihf) arch=armhf cc=arm-linux-gnueabihf ;;
*) arch= ;;
esac
if [ -n "$arch" ] && [ "$(dpkg --print-architecture)" = amd64 ]; then
    dpkg --add-architecture "$arch"
    sed -i 's/^deb http/deb [arch=amd64] http/' /etc/apt/sources.list
    for s in jammy jammy-updates jammy-security; do
        echo "deb [arch=$arch] http://ports.ubuntu.com/ubuntu-ports $s main universe"
    done > "/etc/apt/sources.list.d/ports-$arch.list"
    pkgs+=(gcc-$cc libc6-dev-$arch-cross libpulse-dev:$arch libasound2-dev:$arch)
else
    pkgs+=(libpulse-dev libasound2-dev)
fi
apt-get update
apt-get install -y --no-install-recommends "${pkgs[@]}"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain "$(cat "$(dirname "$0")/../.rust-version")" --target "$target"
