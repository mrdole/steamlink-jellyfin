# Third-party components

This repository contains only original source code (GPL-3.0-or-later). The build downloads or uses the following;
nothing below is committed here.

| Component | Used for | Licence | How it gets in |
|---|---|---|---|
| [FFmpeg](https://ffmpeg.org) 7.1.x | Demuxing, AAC/AC3/E-AC3/MP3 decoding, HTTP(S) in `sljf-play` | LGPL-3.0-or-later (built with `--enable-version3`, no GPL parts) | Downloaded and built by `scripts/build.sh`, statically linked |
| [Mbed TLS](https://github.com/Mbed-TLS/mbedtls) 3.6.x | TLS for FFmpeg | Apache-2.0 | Downloaded and built by `scripts/build.sh`, statically linked |
| [Valve Steam Link SDK](https://github.com/ValveSoftware/steamlink-sdk) | Toolchain, SLVideo/SLAudio, Qt 5.14, SDL2, FreeType | Valve's SDK terms; Qt LGPL-3.0; SDL2 zlib; FreeType FTL | Cloned by the user; libraries are on the Link already, not redistributed |
| Rust crates: rustls, ring, tungstenite, ureq, serde_json, webpki-roots and dependencies | `sljfd` | MIT / Apache-2.0 / ISC (see `daemon/Cargo.lock`) | Fetched by cargo, statically linked |
| Noto Sans | Subtitles and app font | SIL Open Font License 1.1 | Copied from the build machine by the build scripts |
| CA certificates | HTTPS roots for the player | MPL-2.0 (Mozilla CA list) | Copied from the build machine |

If you distribute built binaries, include this notice, the licences above, and this repository's source and build
scripts (which lets recipients rebuild and relink the LGPL FFmpeg).

Steam Link is a trademark of Valve Corporation. Jellyfin is a trademark of the Jellyfin project. This project is not
affiliated with either.
