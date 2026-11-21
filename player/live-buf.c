/* Live TV cushion and picture watch: see live-buf.h. */
#include "live-buf.h"

double livebuf_target(double prebuf, int rebuffers) {
    if (rebuffers <= 0 || prebuf >= LIVEBUF_MAX_S) return prebuf;
    double t = prebuf + LIVEBUF_STEP_S * rebuffers;
    return t > LIVEBUF_MAX_S ? LIVEBUF_MAX_S : t;
}

double livebuf_ahead(double target) {
    return target + 1.0 > LIVEBUF_AHEAD_S ? target + 1.0 : LIVEBUF_AHEAD_S;
}

void picwatch_reset(struct picwatch *w, long frames, double now) { w->ok_t = now; w->frames = frames; }

int picwatch_stopped(struct picwatch *w, unsigned vq_ms, long frames, int feeding, double now) {
    if (vq_ms > 0 || frames != w->frames || !feeding) { picwatch_reset(w, frames, now); return 0; }
    return now - w->ok_t >= LIVEBUF_DRY_S;
}
