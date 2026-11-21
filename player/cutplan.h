/* sljf-play --channel: what to do at a cut between two queued items, and when to open the next one.
 * Plain C99, host-tested (player/test/cutplan-test.c). GPL, like the rest of steamlink-jellyfin.
 *
 * SLVideo keeps decoding whatever H.264 access units it is given, and every item's stream starts with SPS/PPS + IDR
 * (Jellyfin's stream.ts with copied video starts at a keyframe), so an item of the same size goes on in the same
 * decoder stream with no gap (CUT_SAME); another size needs the stream freed and made again (CUT_RESET); a card
 * (silent static with text) feeds no video at all (CUT_CARD). */
#ifndef SLJF_CUTPLAN_H
#define SLJF_CUTPLAN_H

#define CUT_OPEN_AHEAD_S 20.0   /* open the next item when the current one has less than this left to read */

enum cut { CUT_SAME, CUT_RESET, CUT_CARD };

/* the cut from an item of cur_w x cur_h to the next (nxt_w x nxt_h, or a card) */
enum cut cut_kind(int cur_w, int cur_h, int nxt_w, int nxt_h, int nxt_is_card);

/* open the next item now? cur_read_ahead_s = seconds of the current item still to be read (its limit or length minus
   what its reader has read; very large when unknown), cur_rd_eof = its reader hit the end; the next must be queued
   and not opened yet */
int item_should_open_next(double cur_read_ahead_s, int cur_rd_eof, int nxt_queued, int nxt_opened);
#endif
