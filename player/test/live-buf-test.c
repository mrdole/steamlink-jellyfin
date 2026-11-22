#define _POSIX_C_SOURCE 200809L
/* Host test for player/live-buf.c:  cc -std=c99 -Wall -Wextra player/test/live-buf-test.c player/live-buf.c */
#include "../live-buf.h"
#include <stdio.h>
#include <stdlib.h>

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); fails++; } } while (0)

int main(void) {
    /* read-ahead cap: well past the target, so faster-than-real-time stretches build a cushion */
    CHECK(livebuf_ahead(3.0) == LIVEBUF_AHEAD_S);
    CHECK(livebuf_ahead(20.0) == 21.0);

    /* SLJF_READAHEAD_S: honoured, clamped to 1..120, garbage = default 12 */
    setenv("SLJF_READAHEAD_S", "60", 1);  CHECK(livebuf_ahead(3.0) == 60.0);
    setenv("SLJF_READAHEAD_S", "500", 1); CHECK(livebuf_ahead(3.0) == 120.0);
    setenv("SLJF_READAHEAD_S", "0.2", 1); CHECK(livebuf_ahead(0.0) == 1.0);
    setenv("SLJF_READAHEAD_S", "abc", 1); CHECK(livebuf_ahead(3.0) == 12.0);
    setenv("SLJF_READAHEAD_S", "", 1);    CHECK(livebuf_ahead(3.0) == 12.0);
    setenv("SLJF_READAHEAD_S", "-5", 1);  CHECK(livebuf_ahead(3.0) == 12.0);
    setenv("SLJF_READAHEAD_S", "60", 1);  CHECK(livebuf_ahead(80.0) == 81.0);   /* prebuffer still wins when larger */
    unsetenv("SLJF_READAHEAD_S");
    CHECK(livebuf_ahead(3.0) == 12.0);

    /* picture watch */
    struct picwatch w = {0};
    picwatch_reset(&w, 0, 0.0);
    CHECK(!picwatch_stopped(&w, 500, 10, 1, 0.5));   /* queue holds frames */
    CHECK(!picwatch_stopped(&w, 0, 10, 1, 1.0));     /* queue empty 0.5 s: not yet */
    CHECK(picwatch_stopped(&w, 0, 10, 1, 1.3));      /* empty and no new frame for 0.8 s */
    /* frames trickle in just in time: the decoder's queue reads 0 but the picture moves = not stopped */
    picwatch_reset(&w, 10, 2.0);
    for (int i = 1; i <= 20; i++) CHECK(!picwatch_stopped(&w, 0, 10 + i, 1, 2.0 + i * 0.3));
    /* not feeding on purpose (the first prebuffer): never "stopped" */
    picwatch_reset(&w, 30, 10.0);
    CHECK(!picwatch_stopped(&w, 0, 30, 0, 12.0));
    CHECK(!picwatch_stopped(&w, 0, 30, 1, 12.5));    /* the hold restarted the clock */
    CHECK(picwatch_stopped(&w, 0, 30, 1, 13.3));

    if (fails) return 1;
    printf("live-buf: ok\n");
    return 0;
}
