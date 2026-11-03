#!/bin/sh
# Builds ../ffmpeg.exe from source in Docker. Takes about ten minutes.
set -e
cd "$(dirname "$0")"

DOCKER=${DOCKER:-docker}
$DOCKER build -q -t miyav-ffmpeg . >/dev/null
rm -rf out && mkdir out
$DOCKER run --rm -v "$PWD/out:/out" miyav-ffmpeg sh -c "/bin/sh /build/build-ffmpeg.sh && chown -R $(id -u):$(id -g) /out"
cp out/ffmpeg.exe ..
echo "ffmpeg.exe updated"
