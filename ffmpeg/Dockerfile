# Cross-compiles a trimmed ffmpeg.exe for Windows. See build-ffmpeg.sh for why
# we build our own rather than shipping a stock "essentials" build.
#
# llvm-mingw rather than Ubuntu's mingw-w64 packages: gfxcapture (window
# capture) is C++17 against WinRT, and needs current mingw-w64 headers with a
# C++ runtime and thread library built to match. Patching newer headers over
# Ubuntu's GCC built an ffmpeg that crashed as soon as capture started;
# llvm-mingw ships all of those as one consistent release.
FROM mstorsjo/llvm-mingw:latest

RUN apt-get update && apt-get install -y --no-install-recommends \
      nasm pkg-config git curl ca-certificates xz-utils make meson ninja-build \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY build-ffmpeg.sh ddagrab-reconnect.patch /build/
CMD ["/bin/sh", "/build/build-ffmpeg.sh"]
