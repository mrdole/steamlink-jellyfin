/* Live TV cushion: how long sljf-play holds the decoders back after the picture ran dry, how far its reader reads
 * ahead, and when the picture has really stopped. Plain C99, host-tested (player/test/live-buf-test.c).
 * GPL, like the rest of steamlink-jellyfin.
 *
 * Why (2026-11-21, ch 11 on the living-room Link): the Live TV chain (ErsatzTV -> Jellyfin -> Cloudflare) delivers at
 * real time on average but in uneven stretches, and once it falls behind it gives back only a few percent faster than
 * real time. With the old fixed 3 s prebuffer and a 4 s read-ahead cap the player, once dry, played every packet the
 * moment it came: the decoder's queue sat at 0, the static flashed up to 6 times a minute and stayed that way. Now the
 * player stops feeding when the picture runs dry, waits under the static until a bigger cushion is read ahead (each
 * run-out a bit bigger), and lets the reader keep up to LIVEBUF_AHEAD_S, so the next uneven stretch is absorbed. */
#ifndef SLJF_LIVE_BUF_H
#define SLJF_LIVE_BUF_H

#define LIVEBUF_STEP_S 2.0     /* each run-out holds for this much more cushion than the one before */
#define LIVEBUF_MAX_S 8.0      /* ... up to this (seconds of static while it fills, at real-time delivery) */
#define LIVEBUF_AHEAD_S 12.0   /* the reader keeps reading while less than this is read ahead */
#define LIVEBUF_DRY_S 0.7      /* picture stopped: video queue empty and no frame fed for this long */

/* cushion to hold for after `rebuffers` run-outs (0 = the first prebuffer) */
double livebuf_target(double prebuf, int rebuffers);

/* how far the reader may read ahead while holding for / playing with `target` */
double livebuf_ahead(double target);

/* Picture watch. The decoder's queued-ms reads 0 while frames still come in just in time (the picture moves, a little
 * unevenly), so "stopped" also needs no new frame fed for LIVEBUF_DRY_S. `feeding` = 0 while the player holds the
 * decoders back on purpose (prebuffer, rebuffer): that never counts as stopped. */
struct picwatch { double ok_t; long frames; };
void picwatch_reset(struct picwatch *w, long frames, double now);
int picwatch_stopped(struct picwatch *w, unsigned vq_ms, long frames, int feeding, double now);
#endif
