#!/usr/bin/env bash
# Build sljf-play and sljfd for the Steam Link with Valve's SDK (cross-compile on a Linux x86-64 machine, no sudo).
#   scripts/build.sh            -> out/sljf-play, out/sljfd, out/ca.pem, out/sub.ttf
# Rust for sljfd: rustup in ~/.cargo with target armv7-unknown-linux-musleabihf (rustup target add ...).
# Statically links a minimal FFmpeg (demux, H.264 Annex-B filter, AAC/AC3/E-AC3/MP3 decoders, https) and mbedTLS,
# because the Link's own OpenSSL (1.0.1c) and CA list (2013) are too old for Cloudflare in front of Jellyfin.
# SDK: $SDK, default ~/steamlink-sdk (git clone --depth 1 https://github.com/ValveSoftware/steamlink-sdk); sources and
# build trees go in $SDK/build-sljf/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
SDK=${SDK:-$HOME/steamlink-sdk}
[ -f "$SDK/setenv.sh" ] || { echo "Steam Link SDK not found at $SDK (set SDK=...; see docs/INSTALL.md)" >&2; exit 1; }
W=$SDK/build-sljf; mkdir -p "$W" "$HERE/out"
VERSION=$(cat "$HERE/VERSION")
grep -q "^version = \"$VERSION\"" "$HERE/daemon/Cargo.toml" || { echo "VERSION ($VERSION) and daemon/Cargo.toml differ: run scripts/bump-version" >&2; exit 1; }
MBEDTLS=3.6.7
FFMPEG=7.1.4
J=$(nproc)

set +u   # setenv.sh reads unset variables
# shellcheck disable=SC1091
source "$SDK/setenv.sh" >/dev/null
set -u
PREFIX=$W/prefix
CFLAGS_ARM="-O2 -march=armv7-a -mfpu=neon -mfloat-abi=hard"

fetch() {  # url file sha256 (empty = print it, so it can be pinned here)
  [ -f "$W/$2" ] || curl -fL --retry 3 -o "$W/$2" "$1"
  local got; got=$(sha256sum "$W/$2" | cut -d' ' -f1)
  if [ -n "$3" ] && [ "$got" != "$3" ]; then echo "checksum mismatch for $2: $got" >&2; exit 1; fi
  [ -n "$3" ] || echo "sha256 $2 $got (not pinned yet)"
}

# --- mbedTLS (static) ---
if [ ! -f "$PREFIX/lib/libmbedtls.a" ]; then
  fetch "https://github.com/Mbed-TLS/mbedtls/releases/download/mbedtls-$MBEDTLS/mbedtls-$MBEDTLS.tar.bz2" \
        "mbedtls-$MBEDTLS.tar.bz2" "${MBEDTLS_SHA:-a7e8bcbec0e6f761b4af24f25677626b35f762f68eef79c08677a363212d11f6}"
  rm -rf "$W/mbedtls-$MBEDTLS" "$W/mbedtls-build"; tar -C "$W" -xjf "$W/mbedtls-$MBEDTLS.tar.bz2"
  # Entropy: kernel 3.8 has no getrandom(), so mbedTLS falls back to a device file, and its default is /dev/random,
  # which blocks for minutes on the Link (entropy_avail ~50). The handshake then stalled until Cloudflare closed the
  # idle connection and the write failed (-0x4e = NET_SEND_FAILED). /dev/urandom fixes it (found 2026-11-19).
  # TLS 1.2 only: kept from earlier testing (it was not the cause); Cloudflare accepts 1.2 and it is cheaper here.
  CFG="python3 $W/mbedtls-$MBEDTLS/scripts/config.py -f $W/mbedtls-$MBEDTLS/include/mbedtls/mbedtls_config.h"
  $CFG unset MBEDTLS_SSL_PROTO_TLS1_3
  $CFG set MBEDTLS_PLATFORM_DEV_RANDOM '"/dev/urandom"'
  cmake -S "$W/mbedtls-$MBEDTLS" -B "$W/mbedtls-build" -DCMAKE_TOOLCHAIN_FILE="$CMAKE_TOOLCHAIN_FILE" \
        -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" -DENABLE_TESTING=OFF -DENABLE_PROGRAMS=OFF \
        -DUSE_SHARED_MBEDTLS_LIBRARY=OFF -DCMAKE_C_FLAGS="$CFLAGS_ARM" >/dev/null
  cmake --build "$W/mbedtls-build" -j"$J" >/dev/null
  cmake --install "$W/mbedtls-build" >/dev/null
fi

# --- FFmpeg (static, only what the player needs) ---
if [ ! -f "$PREFIX/lib/libavformat.a" ]; then
  fetch "https://ffmpeg.org/releases/ffmpeg-$FFMPEG.tar.xz" "ffmpeg-$FFMPEG.tar.xz" "${FFMPEG_SHA:-71f4aac3573ed9060489cb62526a6c7dda815ae10993789611acd7be9fa9fbf4}"
  rm -rf "$W/ffmpeg-$FFMPEG"; tar -C "$W" -xJf "$W/ffmpeg-$FFMPEG.tar.xz"
  ( cd "$W/ffmpeg-$FFMPEG"
    # setenv.sh puts --sysroot and -rpath-link into CC; FFmpeg wants a bare compiler name + flags
    ./configure --prefix="$PREFIX" --enable-cross-compile --cross-prefix="$CROSS" --arch=arm --target-os=linux \
      --sysroot="$MARVELL_ROOTFS" --extra-cflags="$CFLAGS_ARM -I$PREFIX/include" --extra-ldflags="-L$PREFIX/lib" \
      --pkg-config=false --disable-shared --enable-static --disable-programs --disable-doc --disable-debug \
      --disable-autodetect --disable-everything --disable-avdevice --disable-avfilter --disable-swscale \
      --enable-version3 --enable-mbedtls --enable-protocol=file,http,https,tcp,tls \
      --enable-demuxer=mpegts,mov,matroska,hls \
      --enable-parser=h264,aac,ac3,mpegaudio \
      --enable-decoder=aac,aac_latm,ac3,eac3,mp3 \
      --enable-bsf=h264_mp4toannexb \
      --enable-swresample >"$W/ffmpeg-configure.log" || { tail -5 ffbuild/config.log; exit 1; }
    make -j"$J" >"$W/ffmpeg-make.log" 2>&1 || { tail -20 "$W/ffmpeg-make.log"; exit 1; }
    make install >/dev/null )
fi

# --- the player ---
eval "set -- $CC"; GCC=$1   # cross gcc; we pass our own sysroot flags
$GCC --sysroot="$MARVELL_ROOTFS" $MARVELL_RPATH_LINK_OPTIONS $CFLAGS_ARM -std=gnu99 -Wall -DSLJF_VERSION="\"$VERSION\"" \
  -I"$PREFIX/include" -I"$MARVELL_ROOTFS/usr/include" -I"$MARVELL_ROOTFS/usr/include/freetype2" \
  -o "$HERE/out/sljf-play" "$HERE/player/sljf-play.c" "$HERE/player/sljf-subs.c" \
  "$HERE/player/sljf-static.c" "$HERE/player/static-gen.c" "$HERE/player/live-buf.c" "$HERE/player/cutplan.c" \
  -L"$PREFIX/lib" -lavformat -lavcodec -lswresample -lavutil -lmbedtls -lmbedx509 -lmbedcrypto \
  -lSLVideo -lSLAudio -lfreetype -lpthread -lm 2>&1 | grep -v "^$" || true
[ -f "$HERE/out/sljf-play" ] || { echo "build failed"; exit 1; }
"$STRIP" "$HERE/out/sljf-play"

# --- the daemon (Rust, static musl; ring's C parts use the SDK gcc through sljfd/arm-cc) ---
( cd "$HERE/daemon" && env -u CC -u CXX -u CPP -u AS -u LD -u LD_LIBRARY_PATH PATH="$HOME/.cargo/bin:$PATH" SDK="$SDK" \
    AR_armv7_unknown_linux_musleabihf="$SDK/toolchain/bin/armv7a-cros-linux-gnueabi-ar" \
    cargo build --quiet --release --target armv7-unknown-linux-musleabihf )
cp "$HERE/daemon/target/armv7-unknown-linux-musleabihf/release/sljfd" "$HERE/out/sljfd"
"$STRIP" "$HERE/out/sljfd"
install -m 644 /etc/ssl/certs/ca-certificates.crt "$HERE/out/ca.pem"   # current CA list for the Link (its own is from 2013)
install -m 644 "$(fc-match -f '%{file}' 'Noto Sans:bold')" "$HERE/out/sub.ttf"   # subtitle font (Noto Sans Bold, OFL)
cp "$HERE/VERSION" "$HERE/out/VERSION"
echo "built $VERSION"
ls -l "$HERE/out"
