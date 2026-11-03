#!/bin/sh
# Stage the slice of OBS Studio the recorder runs on (see rust/miyav-obs) into
# obs/stage/obs, laid out as OBS lays itself out: bin/64bit, core/<plugin>,
# data/libobs.
#
# The files are the official release's, unmodified: OBS's game-capture hook is
# only safe to inject into anti-cheat-protected games because it is OBS's
# signed DLL, so nothing here may be rebuilt or patched. Only the plugins the
# replay buffer uses are taken, and English text only.
set -e
cd "$(dirname "$0")"

OBS_VERSION=33.0.0
OBS_SHA256=3369a3ecd67b5669a8a08d77357f87cc786d1fba42fbbd1054d841a69f6271a7
ZIP="OBS-Studio-$OBS_VERSION-Windows-x64.zip"

mkdir -p cache
if ! echo "$OBS_SHA256  cache/$ZIP" | sha256sum -c --status 2>/dev/null; then
  curl -fL --retry 3 -o "cache/$ZIP" \
    "https://github.com/obsproject/obs-studio/releases/download/$OBS_VERSION/$ZIP"
  echo "$OBS_SHA256  cache/$ZIP" | sha256sum -c
fi

rm -rf stage unpacked
mkdir -p stage/obs unpacked
unzip -q "cache/$ZIP" -d unpacked
src=unpacked
dst=stage/obs

mkdir -p "$dst/bin/64bit"
# libobs and what it and the plugins below load. libcurl: win-capture links
# it (its compatibility update, which miyav-obs keeps off the network);
# avdevice, srt and librist: obs-ffmpeg links them.
for f in obs.dll libobs-d3d11.dll libobs-winrt.dll w32-pthreads.dll zlib.dll \
         avcodec-62.dll avformat-62.dll avutil-60.dll swscale-9.dll swresample-6.dll \
         avfilter-11.dll avdevice-62.dll libx264-164.dll srt.dll librist.dll libcurl.dll \
         obs-ffmpeg-mux.exe obs-nvenc-test.exe obs-amf-test.exe; do
  cp "$src/bin/64bit/$f" "$dst/bin/64bit/"
done

mkdir -p "$dst/data"
cp -r "$src/data/libobs" "$dst/data/"

for m in win-capture win-wasapi obs-ffmpeg obs-nvenc obs-x264; do
  mkdir -p "$dst/core/$m"
  cp "$src/core/$m/$m.dll" "$dst/core/$m/"
  if [ -d "$src/core/$m/data" ]; then
    cp -r "$src/core/$m/data" "$dst/core/$m/"
    find "$dst/core/$m/data" -name '*.pdb' -delete
    if [ -d "$dst/core/$m/data/locale" ]; then
      find "$dst/core/$m/data/locale" -type f ! -name en-US.ini -delete
    fi
  fi
done

cp NOTICE.txt "$dst/NOTICE.txt"
rm -rf unpacked
echo "staged $(find stage/obs -type f | wc -l) files, $(du -sh stage/obs | cut -f1)"
