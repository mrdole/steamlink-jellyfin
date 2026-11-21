/* Analogue "TV snow" pixels and a faint hiss, for the no-signal static overlay.
 * Plain C99, no libm. GPL, like the rest of steamlink-jellyfin. */
#ifndef SLJF_STATIC_GEN_H
#define SLJF_STATIC_GEN_H
#include <stdint.h>

#define STATIC_W 320
#define STATIC_H 180

/* Fill w x h ARGB pixels (A=0xFF, R=G=B grey) with chunky snow. pitch_bytes is
 * the row stride in bytes. *seed is the xorshift32 state, advanced in place
 * (0 is replaced by a non-zero value). */
void static_noise(uint32_t *px, int w, int h, int pitch_bytes, uint32_t *seed);

struct hiss { uint32_t seed; float lp; };

/* Write `frames` interleaved stereo S16 frames (48 kHz) of low-passed white
 * noise. Peak |sample| <= 32767 * volume_pct / 100 (pct clamped to 0..100);
 * 0 gives silence. Zero-initialised struct hiss is fine. */
void static_hiss(int16_t *lr, int frames, int volume_pct, struct hiss *h);
#endif
