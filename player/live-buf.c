/* Live TV read-ahead and picture watch: see live-buf.h. */
#include "live-buf.h"
#include <stdlib.h>

double livebuf_ahead(double prebuf) {
    double base = LIVEBUF_AHEAD_S;
    const char *e = getenv("SLJF_READAHEAD_S");   /* sljfd sets it per item (channels: 20 with a stick, 60 without) */
    if (e && *e) {
        char *end; double v = strtod(e, &end);
        if (end != e && *end == 0 && v > 0) base = v < LIVEBUF_AHEAD_MIN_S ? LIVEBUF_AHEAD_MIN_S : v > LIVEBUF_AHEAD_MAX_S ? LIVEBUF_AHEAD_MAX_S : v;
    }
    return prebuf + 1.0 > base ? prebuf + 1.0 : base;
}

void picwatch_reset(struct picwatch *w, long frames, double now) { w->ok_t = now; w->frames = frames; }

int picwatch_stopped(struct picwatch *w, unsigned vq_ms, long frames, int feeding, double now) {
    if (vq_ms > 0 || frames != w->frames || !feeding) { picwatch_reset(w, frames, now); return 0; }
    return now - w->ok_t >= LIVEBUF_DRY_S;
}
