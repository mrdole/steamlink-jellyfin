/*
 * sljf-static: analogue "no signal" static for Live TV (SLJF_STATIC=<hiss volume 0-100>).
 *
 * Shown while a channel tunes (sljf-play --static waits for the URL with it up, then opens and prebuffers under it)
 * and again when the video has run dry mid-stream; hidden as soon as real picture is on screen (sljf-play decides
 * when, see its main loop). Never over real picture or sound.
 *
 * Picture: its own full-screen overlay (STATIC_W x STATIC_H grey snow from static-gen.c, scaled up by the overlay
 * hardware), redrawn about 15 times a second into two overlays in turn (no tearing). The Link shows one overlay at a
 * time, so the subtitle / bar strip (sljf-subs.c) is suspended while the static is up.
 *
 * Sound: a faint low-passed hiss (scaled by the player's volume like the programme sound), 20 ms chunks topped up while less than 60 ms is queued (so at most 80 ms of it
 * ever sits in the queue). Before the player has an audio stream, the static makes its own 48 kHz stereo stream and
 * hands it to the player (static_give_audio), so programme audio follows in the same stream (the Link has one
 * output; no second stream). Hiss in the programme stream counts in SLAudio's queued samples exactly like the
 * sync code's inserted silence, so the player's audio clock (end of decoded audio minus what is queued) stays right.
 * The hiss stops for good (until the next show) once the player queues programme sound (static_prog_audio).
 */
#include "sljf-static.h"
#include "static-gen.h"
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

void subs_suspend(int on);         /* sljf-subs.c */
int subs_text_mask(const char *text, uint8_t *mask, int w, int h);

#define FRAME_S (1.0 / 15)          /* new snow this often */
#define HISS_RATE 48000
#define HISS_CHUNK_FRAMES (STATIC_AUDIO_CHUNK_BYTES / 4)
#define HISS_TARGET_MS 60           /* top up while less than this is queued */

static CSLVideoContext *g_vctx;
static CSLVideoOverlay *g_ov[2];
static int g_ovi, g_vis, g_vol, g_prog;
static int g_broken;               /* no overlay could be made: do not try (and log) on every pass */
static double g_next;              /* when to draw the next snow */
static uint32_t g_seed = 0x5EEDu;
static struct hiss g_hs;
static CSLAudioContext *g_actx;    /* own context (only while g_own) */
static CSLAudioStream *g_astr;     /* where the hiss goes: own stream or the player's */
static int g_own;
static pthread_t g_th;
static volatile sig_atomic_t g_bg;  /* the background thread runs */
static int g_th_up;
static volatile int32_t g_gain = 65536;   /* the player's volume (SLJF_VOLUME / "volume N"), 1/65536 steps */
static uint8_t g_card[STATIC_W * STATIC_H];   /* a card's text (static_card), coverage 0-255 */
static int g_card_on, g_card_y0, g_card_y1;   /* drawn; the rows of its dark band */

void static_set_gain(int32_t g) { g_gain = g; }

static double mono(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

int static_volume(void) {
    const char *e = getenv("SLJF_STATIC");
    if (!e || !*e) return -1;
    int v = atoi(e);
    return v < 0 ? 0 : v > 100 ? 100 : v;
}

static void draw(void) {
    g_ovi ^= 1;
    uint32_t *px = NULL; int pitch = 0;
    SLVideo_GetOverlayPixels(g_ov[g_ovi], &px, &pitch);
    if (!px) return;
    static_noise(px, STATIC_W, STATIC_H, pitch, &g_seed);
    for (int y = g_card_on ? g_card_y0 : STATIC_H; y < g_card_y1; y++) {   /* a card: white text on a darker band */
        uint32_t *row = (uint32_t *)((uint8_t *)px + y * pitch);
        const uint8_t *m = g_card + y * STATIC_W;
        for (int x = 0; x < STATIC_W; x++) {
            uint32_t v = (row[x] & 0xFF) / 3;
            v += (255 - v) * m[x] / 255;
            row[x] = 0xFF000000u | v << 16 | v << 8 | v;
        }
    }
    SLVideo_ShowOverlay(g_ov[g_ovi]);
}

void static_card(const char *text) {
    g_card_on = 0;
    if (!text || subs_text_mask(text, g_card, STATIC_W, STATIC_H)) return;
    int y0 = STATIC_H, y1 = 0;
    for (int y = 0; y < STATIC_H; y++)
        for (int x = 0; x < STATIC_W; x++) if (g_card[y * STATIC_W + x]) { if (y < y0) y0 = y; y1 = y + 1; break; }
    if (y1 <= y0) return;                          /* empty text: plain static */
    g_card_y0 = y0 - 6 < 0 ? 0 : y0 - 6;
    g_card_y1 = y1 + 6 > STATIC_H ? STATIC_H : y1 + 6;
    g_card_on = 1;
}

int static_show(CSLVideoContext *v, int volume) {
    if (g_vis) return 0;
    if (g_broken) return -1;
    if (!g_ov[0]) {
        g_vctx = v;
        for (int i = 0; i < 2; i++) {
            g_ov[i] = SLVideo_CreateOverlay(g_vctx, STATIC_W, STATIC_H);
            if (!g_ov[i]) { printf("static: SLVideo_CreateOverlay failed\n"); static_free(); g_broken = 1; return -1; }
            SLVideo_SetOverlayDisplayArea(g_ov[i], 0.0f, 0.0f, 1.0f, 1.0f);
        }
    }
    g_vol = volume < 0 ? 0 : volume > 100 ? 100 : volume;
    if (g_vol > 0 && !g_astr) {        /* no programme stream yet: one of our own */
        g_actx = SLAudio_CreateContext();
        g_astr = g_actx ? SLAudio_CreateStream(g_actx, HISS_RATE, 2, STATIC_AUDIO_CHUNK_BYTES, 0) : NULL;
        if (g_astr) g_own = 1;
        else { printf("static: no audio stream, silent\n"); if (g_actx) SLAudio_FreeContext(g_actx); g_actx = NULL; }
    }
    g_prog = 0;
    subs_suspend(1);                   /* one overlay at a time: the strip goes */
    draw();
    g_vis = 1;
    g_next = mono() + FRAME_S;
    printf("static on\n");
    static_tick(mono());               /* the first hiss right away */
    return 0;
}

void static_tick(double now_s) {
    if (!g_vis) return;
    if (now_s >= g_next) {
        draw();
        g_next += FRAME_S;
        if (g_next < now_s) g_next = now_s + FRAME_S;   /* fell behind (a slow pass): no catch-up burst */
    }
    if (!g_astr || !g_vol || g_prog) return;
    uint32_t q = SLAudio_GetQueuedAudioSamples(g_astr);
    while (q * 1000 / HISS_RATE < HISS_TARGET_MS) {
        int16_t *buf = (int16_t *)SLAudio_BeginFrame(g_astr);
        if (!buf) break;
        static_hiss(buf, HISS_CHUNK_FRAMES, g_vol, &g_hs);
        int32_t g = g_gain;            /* the hiss follows the volume, so it stays below the programme (and mutes) */
        if (g < 65536) for (int i = 0; i < HISS_CHUNK_FRAMES * 2; i++) buf[i] = (int16_t)((buf[i] * g) >> 16);
        SLAudio_SubmitFrame(g_astr);
        q += HISS_CHUNK_FRAMES;
    }
}

void static_hide(void) {
    if (!g_vis) return;
    SLVideo_HideOverlay(g_ov[g_ovi]);
    g_vis = 0;
    subs_suspend(0);                   /* the strip may come back now */
    printf("static off\n");
}

int static_visible(void) { return g_vis; }

void static_give_audio(CSLAudioContext **c, CSLAudioStream **s) {
    if (g_own && g_astr) {
        if (!*s) {                     /* ours becomes the programme stream */
            *c = g_actx; *s = g_astr;
            g_actx = NULL; g_own = 0;
            return;
        }
        SLAudio_FreeStream(g_astr);    /* both made one (not expected): keep the player's */
        SLAudio_FreeContext(g_actx);
        g_actx = NULL; g_own = 0;
    }
    g_astr = *s;
}

void static_prog_audio(void) { g_prog = 1; }

static void *bg(void *o) {
    (void)o;
    while (g_bg) { static_tick(mono()); usleep(10000); }
    return NULL;
}

void static_background(int on) {
    if (on && !g_th_up && g_vis) {
        g_bg = 1;
        if (pthread_create(&g_th, NULL, bg, NULL) == 0) g_th_up = 1;
        else g_bg = 0;
    } else if (!on && g_th_up) {
        g_bg = 0;
        pthread_join(g_th, NULL);
        g_th_up = 0;
    }
}

void static_free(void) {
    static_background(0);
    for (int i = 0; i < 2; i++)
        if (g_ov[i]) { SLVideo_HideOverlay(g_ov[i]); SLVideo_FreeOverlay(g_ov[i]); g_ov[i] = NULL; }
    g_vis = 0;
    if (g_own) { SLAudio_FreeStream(g_astr); SLAudio_FreeContext(g_actx); g_own = 0; }
    g_astr = NULL; g_actx = NULL;
}
