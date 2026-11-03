#!/bin/sh
# Builds ffmpeg.exe into /out. Run through ffmpeg/build.sh, not directly.
#
# Why our own build:
# - Stock builds track the newest NVENC SDK, so they refuse to encode on any
#   NVIDIA driver older than that SDK (ffmpeg 9 wants 610+). Pinning the
#   headers to 12.1 keeps every driver from 531 (early 2023) working.
# - The stock "essentials" build links dozens of codec libraries the app never
#   touches and comes out at ~100 MB. We only need x264 plus the hardware
#   encoders; everything else uses ffmpeg's built-in codecs.
set -e

FFMPEG_VERSION=9.0.1
NVHEADERS=n12.1.14.0
HOST=x86_64-w64-mingw32
P=/build/prefix
J=$(nproc)

mkdir -p "$P" /out
export PKG_CONFIG_PATH="$P/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="$P/lib/pkgconfig"
cd /build

echo "==> nv-codec-headers $NVHEADERS"
git clone -q --depth 1 -b "$NVHEADERS" https://github.com/FFmpeg/nv-codec-headers
make -C nv-codec-headers PREFIX="$P" install >/dev/null

echo "==> AMF headers"
git clone -q --depth 1 --filter=blob:none --sparse https://github.com/GPUOpen-LibrariesAndSDKs/AMF amf
git -C amf sparse-checkout set amf/public/include
mkdir -p "$P/include/AMF"
cp -r amf/amf/public/include/* "$P/include/AMF/"

echo "==> x264"
git clone -q --depth 1 https://code.videolan.org/videolan/x264.git
cd x264
CC=$HOST-clang ./configure --host=$HOST --cross-prefix=$HOST- --prefix="$P" \
  --enable-static --disable-cli --enable-pic >/dev/null
make -j"$J" >/dev/null
make install >/dev/null
cd ..

# ffmpeg's own AV1 decoder only drives the GPU's, and refuses to run without one
# it can use. AV1 clips still need thumbnails and re-encodes.
echo "==> dav1d"
git clone -q --depth 1 -b 1.5.1 https://code.videolan.org/videolan/dav1d.git
cat > cross.txt <<EOF
[binaries]
c = '$HOST-clang'
cpp = '$HOST-clang++'
ar = '$HOST-ar'
strip = '$HOST-strip'
[host_machine]
system = 'windows'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
meson setup dav1d/build dav1d --cross-file cross.txt --prefix="$P" --libdir=lib \
  --default-library=static --buildtype=release -Denable_tools=false -Denable_tests=false >/dev/null
ninja -C dav1d/build install >/dev/null

echo "==> ffmpeg $FFMPEG_VERSION"
curl -fsSL "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" | tar xJ
cd "ffmpeg-$FFMPEG_VERSION"
# ddagrab gives up for good when Windows takes the duplication away - which a
# fullscreen game does on every alt-tab - and with dup_frames=0 it waits forever
# on a screen that stops changing. The patch reopens a lost duplication and
# repeats the last frame after 200ms without a new one, as OBS does, so the
# recording carries on with no gap.
patch -p1 < /build/ddagrab-reconnect.patch
# configure links gfxcapture with -lstdc++, but llvm-mingw's C++ runtime is
# libc++. An empty libstdc++.a satisfies the flag; -lc++ brings the real one.
$HOST-ar rc "$P/lib/libstdc++.a"
./configure \
  --prefix=/out --arch=x86_64 --target-os=mingw32 --cross-prefix=$HOST- \
  --cc=$HOST-clang --cxx=$HOST-clang++ \
  --pkg-config=pkg-config --pkg-config-flags=--static \
  --extra-cflags="-I$P/include" --extra-ldflags="-L$P/lib -static" --extra-libs="-lc++" \
  --enable-gpl --enable-version3 \
  --enable-libx264 --enable-libdav1d --enable-ffnvcodec --enable-nvenc --enable-amf --enable-d3d11va \
  --disable-ffplay --disable-ffprobe --disable-doc --disable-debug --disable-network \
  > /out/configure.log

# The app depends on these; stop now rather than compile for ten minutes and
# ship without them. Why a feature was left out is in ffbuild/config.log.
for need in gfxcapture ddagrab h264_nvenc hevc_nvenc av1_nvenc h264_amf libx264 libdav1d; do
  grep -q "$need" /out/configure.log || {
    echo "MISSING: $need"; grep -n -B2 -A12 "$need" ffbuild/config.log | tail -40; exit 1; }
done

make -j"$J" >/dev/null
$HOST-strip -o /out/ffmpeg.exe ffmpeg.exe
cp COPYING.GPLv3 /out/ffmpeg-LICENSE.txt
ls -l /out/ffmpeg.exe
