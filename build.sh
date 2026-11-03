#!/bin/sh
# Builds Miyav on Linux, into dist/: the setup and the portable zip.
#
# Needs Docker, makensis, zip, unzip and curl. Set DOCKER="sudo docker" if your user
# cannot run Docker directly. CARGO_CACHE and CARGO_OUT choose where crates and build
# output are kept between builds.
set -e
cd "$(dirname "$0")"

VERSION=$(cat VERSION)
WIN=x86_64-pc-windows-gnu
DOCKER=${DOCKER:-docker}
CARGO_CACHE=${CARGO_CACHE:-$HOME/.cache/miyav/cargo}
CARGO_OUT=${CARGO_OUT:-$HOME/.cache/miyav/target}
mkdir -p "$CARGO_CACHE" "$CARGO_OUT"

[ -f ffmpeg.exe ] || ffmpeg/build.sh

echo "==> OBS files"
[ -d obs/stage/obs/core/win-capture ] || obs/fetch.sh

echo "==> exes"
$DOCKER build -q -t miyav-build rust >/dev/null
$DOCKER run --rm -v "$PWD:/src" -v "$CARGO_CACHE:/usr/local/cargo/registry" -v "$CARGO_OUT:/target" \
  -e CARGO_TARGET_DIR=/target -e MIYAV_VERSION="$VERSION" miyav-build sh -c "
    cd /src/rust && touch src/server.rs && cargo build --release --target $WIN &&
    cd miyav-obs && CARGO_TARGET_DIR=/target/helper cargo build --release --target $WIN &&
    cp /target/$WIN/release/Miyav.exe /src/ &&
    cp /target/helper/$WIN/release/miyav-obs.exe /src/obs/stage/obs/bin/64bit/ &&
    cp \$(find /usr/local/cargo/registry -path '*webview2-com-sys*/x64/WebView2Loader.dll' | sort | tail -1) /src/ &&
    chown $(id -u):$(id -g) /src/Miyav.exe /src/WebView2Loader.dll /src/obs/stage/obs/bin/64bit/miyav-obs.exe /src/rust/Cargo.lock /src/rust/miyav-obs/Cargo.lock"

echo "==> packages"
rm -rf dist
mkdir -p dist
makensis -V2 -DVERSION="$VERSION" miyav.nsi
zip -q -9 -j "dist/Miyav-$VERSION-portable.zip" Miyav.exe WebView2Loader.dll ffmpeg.exe LICENSE NOTICE.md
(cd obs/stage && zip -q -9 -r "../../dist/Miyav-$VERSION-portable.zip" obs)
ls -lh dist
