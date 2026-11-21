/* Host test for player/cutplan.c:  cc -std=c99 -Wall -Wextra player/test/cutplan-test.c player/cutplan.c */
#include "../cutplan.h"
#include <stdio.h>

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); fails++; } } while (0)

int main(void) {
    CHECK(cut_kind(1920, 1080, 1920, 1080, 0) == CUT_SAME);
    CHECK(cut_kind(1920, 1080, 1280, 720, 0) == CUT_RESET);
    CHECK(cut_kind(1920, 1080, 0, 0, 1) == CUT_CARD);
    CHECK(cut_kind(706, 480, 1920, 1080, 0) == CUT_RESET);
    CHECK(cut_kind(1920, 1080, 1920, 1080, 1) == CUT_CARD);   /* a card is a card whatever the sizes */

    CHECK(item_should_open_next(30, 0, 1, 0) == 0);   /* plenty left to read */
    CHECK(item_should_open_next(19, 0, 1, 0) == 1);   /* under 20 s left */
    CHECK(item_should_open_next(50, 1, 1, 0) == 1);   /* the reader hit the end (shorter than it said) */
    CHECK(item_should_open_next(5, 0, 0, 0) == 0);    /* nothing queued */
    CHECK(item_should_open_next(5, 0, 1, 1) == 0);    /* already opened */

    if (fails) return 1;
    printf("cutplan: ok\n");
    return 0;
}
