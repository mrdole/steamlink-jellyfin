/* Live TV read-ahead and picture watch: see live-buf.h. */
#include "live-buf.h"

double livebuf_ahead(double prebuf) {
    return prebuf + 1.0 > LIVEBUF_AHEAD_S ? prebuf + 1.0 : LIVEBUF_AHEAD_S;
}

void picwatch_reset(struct picwatch *w, long frames, double now) { w->ok_t = now; w->frames = frames; }

int picwatch_stopped(struct picwatch *w, unsigned vq_ms, long frames, int feeding, double now) {
    if (vq_ms > 0 || frames != w->frames || !feeding) { picwatch_reset(w, frames, now); return 0; }
    return now - w->ok_t >= LIVEBUF_DRY_S;
}
