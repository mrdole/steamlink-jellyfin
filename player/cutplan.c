/* sljf-play --channel cut decisions: see cutplan.h. */
#include "cutplan.h"

enum cut cut_kind(int cur_w, int cur_h, int nxt_w, int nxt_h, int nxt_is_card) {
    if (nxt_is_card) return CUT_CARD;
    return cur_w == nxt_w && cur_h == nxt_h ? CUT_SAME : CUT_RESET;
}

int item_should_open_next(double cur_read_ahead_s, int cur_rd_eof, int nxt_queued, int nxt_opened) {
    if (!nxt_queued || nxt_opened) return 0;
    return cur_rd_eof || cur_read_ahead_s < CUT_OPEN_AHEAD_S;
}
