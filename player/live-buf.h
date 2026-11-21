/* Live TV: how far sljf-play's reader reads ahead, and when the picture has really stopped (for the static).
 * Plain C99, host-tested (player/test/live-buf-test.c). GPL, like the rest of steamlink-jellyfin.
 *
 * Why (2026-11-21, ch 11 on the living-room Link, 0.1.0-alpha.5): the Live TV stream arrives at real time on average
 * but unevenly (the path out of the server's network runs 0.75-0.95x for minutes, then the server catches up at
 * 1.05x). Once the player's cushion had run out it fed every packet the moment it came; the decoder's queued-ms then
 * read 0 although frames kept coming, and "queue empty for 700 ms" flashed the static up to ~6 times a minute
 * (64 statics in 10 min, 57 of them under a second). Holding the decoders back to rebuild a cushion was tried and
 * dropped: a starved SLVideo stream races through what it is given afterwards, and the further behind live the
 * player sits, the more the server skips whole 4 s segments. */
#ifndef SLJF_LIVE_BUF_H
#define SLJF_LIVE_BUF_H

#define LIVEBUF_AHEAD_S 12.0   /* the reader keeps reading while less than this is read ahead (default; SLJF_READAHEAD_S overrides) */
#define LIVEBUF_AHEAD_MIN_S 1.0
#define LIVEBUF_AHEAD_MAX_S 120.0
#define LIVEBUF_DRY_S 0.7      /* picture stopped: video queue empty and no frame fed for this long */

/* how far the reader may read ahead with a prebuffer of `prebuf` seconds: SLJF_READAHEAD_S (1..120, default 12;
   unset, garbage or <= 0 = 12) or prebuf+1, whichever is larger */
double livebuf_ahead(double prebuf);

/* Picture watch. The decoder's queued-ms reads 0 while frames still come in just in time (the picture moves, a little
 * unevenly), so "stopped" also needs no new frame fed for LIVEBUF_DRY_S. `feeding` = 0 while the player holds the
 * decoders back on purpose (the first prebuffer): that never counts as stopped. */
struct picwatch { double ok_t; long frames; };
void picwatch_reset(struct picwatch *w, long frames, double now);
int picwatch_stopped(struct picwatch *w, unsigned vq_ms, long frames, int feeding, double now);
#endif
