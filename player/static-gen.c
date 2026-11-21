#include "static-gen.h"

static uint32_t xs32(uint32_t *s)
{
    uint32_t x = *s ? *s : 0x9E3779B9u;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    return *s = x;
}

void static_noise(uint32_t *px, int w, int h, int pitch_bytes, uint32_t *seed)
{
    int x, y;
    if (!px || !seed) return;
    for (y = 0; y < h; y++) {
        uint32_t *row = (uint32_t *)((uint8_t *)px + (long)y * pitch_bytes);
        /* one draw per 4 pixels (a byte each); a row-wide brightness
         * offset gives the slight horizontal banding */
        int band = (int)(xs32(seed) & 31) - 16;
        for (x = 0; x < w; x += 4) {
            uint32_t r = xs32(seed);
            int k;
            for (k = 0; k < 4 && x + k < w; k++) {
                int v = (int)((r >> (8 * k)) & 0xFF) + band;
                if (v < 0) v = 0;
                if (v > 255) v = 255;
                row[x + k] = 0xFF000000u | ((uint32_t)v << 16) | ((uint32_t)v << 8) | (uint32_t)v;
            }
        }
    }
}

void static_hiss(int16_t *lr, int frames, int volume_pct, struct hiss *h)
{
    int i;
    float amp, lp;
    if (!lr || !h) return;
    if (volume_pct < 0) volume_pct = 0;
    if (volume_pct > 100) volume_pct = 100;
    amp = 32767.0f * (float)volume_pct / 100.0f;
    lp = h->lp;
    for (i = 0; i < frames; i++) {
        int c;
        for (c = 0; c < 2; c++) {
            /* white in [-1,1), one-pole low-pass (a=0.35), gain 1 stays in range */
            float w = (float)(int32_t)xs32(&h->seed) * (1.0f / 2147483648.0f);
            float s;
            int v;
            lp += 0.35f * (w - lp);
            s = lp * amp;
            v = (int)s;
            if (v > (int)amp) v = (int)amp;
            if (v < -(int)amp) v = -(int)amp;
            lr[2 * i + c] = (int16_t)v;
        }
    }
    h->lp = lp;
}
