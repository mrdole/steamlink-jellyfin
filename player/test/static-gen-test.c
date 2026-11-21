#include <stdio.h>
#include <string.h>
#include <math.h>
#include "../static-gen.h"

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); fails++; } } while (0)

static uint32_t a[STATIC_W * STATIC_H], b[STATIC_W * STATIC_H];

int main(void)
{
    uint32_t s1 = 1234, s2 = 1234, z = 0;
    int i, differ = 0, ok = 1;
    static int16_t buf[960 * 2];
    struct hiss h = { 99, 0 };
    double sum = 0;
    int lim = 32767 * 8 / 100;

    static_noise(a, STATIC_W, STATIC_H, STATIC_W * 4, &s1);
    for (i = 0; i < STATIC_W * STATIC_H; i++) {
        uint32_t p = a[i];
        if ((p >> 24) != 0xFF || ((p >> 16) & 0xFF) != ((p >> 8) & 0xFF) || ((p >> 8) & 0xFF) != (p & 0xFF)) ok = 0;
        if (p != a[0]) differ = 1;
    }
    CHECK(ok);
    CHECK(differ);
    static_noise(b, STATIC_W, STATIC_H, STATIC_W * 4, &s2);
    CHECK(memcmp(a, b, sizeof a) == 0);
    s1 = 1234;
    static_noise(b, STATIC_W, STATIC_H, STATIC_W * 4, &s1);
    CHECK(memcmp(a, b, sizeof a) == 0);

    /* seed 0 must not lock up; pitch larger than the row is honoured */
    static_noise(a, 8, 2, 64, &z);
    CHECK(z != 0);
    CHECK(((a[0] >> 24) == 0xFF) && ((a[16] >> 24) == 0xFF));

    static_hiss(buf, 960, 8, &h);
    for (i = 0; i < 960 * 2; i++) {
        CHECK(buf[i] <= lim && buf[i] >= -lim);
        sum += (double)buf[i] * buf[i];
    }
    CHECK(sqrt(sum / (960 * 2)) > 1.0);

    static_hiss(buf, 960, 0, &h);
    for (i = 0; i < 960 * 2; i++) CHECK(buf[i] == 0);

    static_hiss(buf, 960, 500, &h); /* clamped to 100 */
    for (i = 0; i < 960 * 2; i++) CHECK(buf[i] <= 32767 && buf[i] >= -32767);

    if (fails) return 1;
    puts("ok");
    return 0;
}
