/* sljf-play --channel: what to do at a cut between two queued items, and when to open the next one.
 * Plain C99, host-tested (player/test/cutplan-test.c). GPL, like the rest of steamlink-jellyfin.
 *
 * SLVideo keeps decoding whatever H.264 access units it is given, and every item's stream starts with SPS/PPS + IDR
 * (Jellyfin's stream.ts with copied video starts at a keyframe), so an item of the same size goes on in the same
 * decoder stream with no gap (CUT_SAME); another size needs the stream freed and made again (CUT_RESET); a card
 * (silent static with text) feeds no video at all (CUT_CARD). */
#ifndef SLJF_CUTPLAN_H
#define SLJF_CUTPLAN_H

#define CUT_OPEN_AHEAD_S 25.0   /* open the next item when the current one has less than this left to play */

enum cut { CUT_SAME, CUT_RESET, CUT_CARD };

/* the cut from an item of cur_w x cur_h to the next (nxt_w x nxt_h, or a card) */
enum cut cut_kind(int cur_w, int cur_h, int nxt_w, int nxt_h, int nxt_is_card);

/* open the next item now? cur_play_left_s = seconds until the current item's cut (its limit or length minus what has
   been fed of it; very large when unknown): by time to the cut, not by what is left to read, so a big read-ahead
   does not open the next item early and leave its server connection idle; cur_rd_eof = nothing more of it can be
   fed: its reader failed or its limit is reached (sljf-play counts a reader at the end of a stream, which may be
   shorter than it said, through cur_play_left_s); the next must be queued and not opened yet */
int item_should_open_next(double cur_play_left_s, int cur_rd_eof, int nxt_queued, int nxt_opened);
#endif
