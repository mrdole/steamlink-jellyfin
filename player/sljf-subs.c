/*
 * sljf-subs: what sljf-play draws on the SLVideo overlay (the plane above the video): text subtitles and the
 * on-screen bar (title, progress, hints such as "Next episode in 8 s").
 *
 * Subtitles: sljfd downloads the chosen track from Jellyfin as SRT (the server converts ASS/mov_text/WebVTT) and
 * passes the file in SLJF_SUBS. The download runs while playback starts (the server may take ~30 s to extract a track
 * the first time), so a file that is not there yet is looked for once a second and loaded when it appears. Image
 * subtitles (PGS, VobSub) are not handled here: sljfd asks the server to burn those into the video instead.
 *
 * The bar: sljfd writes "osd" lines to sljf-play's stdin (see osd_command); it sits along the bottom edge and the
 * subtitles move up above it while it shows.
 *
 * The Link shows one overlay at a time, so subtitles and bar share one strip, rendered with FreeType only when
 * something on it changes (the bar: once a second for its clock). Two overlays are used in turn so the strip is
 * never shown half drawn.
 *
 * Size: the strip is 1280x260 pixels shown over the bottom 36% of the screen (the Link outputs 1080p, so it is
 * scaled 1.5x); font 40 px there is about 60 px on a 1080p TV, a usual subtitle size.
 */
#include <SLVideo.h>
#include <ft2build.h>
#include FT_FREETYPE_H
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <time.h>

#define SUB_W 1280
#define SUB_H 260
#define SUB_PX 40                 /* font size in strip pixels */
#define SUB_LINE (SUB_PX * 5 / 4)
#define SUB_MARGIN 22             /* above the bottom edge */
#define SUB_MAXW 1180
#define SUB_OUTLINE 3
#define OSD_H 124                 /* the bar: bottom rows of the strip */
#define C_BG    0xE0282828u       /* gruvbox: bar background */
#define C_FG    0xFFEBDBB2u       /* text */
#define C_DIM   0xFFA89984u       /* times */
#define C_ACC   0xFFFABD2Fu       /* progress, hints */
#define C_TRACK 0xFF504945u       /* progress track */

typedef struct { double start, end; char *text; } Cue;

static Cue *g_cues;
static int g_ncues;
static FT_Library g_ft;
static FT_Face g_face;
static CSLVideoOverlay *g_ov[2];
static int g_ovi, g_shown;
static char *g_cur;               /* text on screen now ("" = none) */
static CSLVideoContext *g_vctx;
static const char *g_path;
static int g_ready;               /* cues loaded */
static int g_draw;                /* font and overlays set up (1), failed (-1) */
static time_t g_looked;
static int g_suspended;           /* the static (sljf-static.c) owns the screen: draw nothing */

static double srt_time(const char *s) {
    int h, m, sec, ms;
    if (sscanf(s, "%d:%d:%d%*[,.]%d", &h, &m, &sec, &ms) != 4) return -1;
    return h * 3600 + m * 60 + sec + ms / 1000.0;
}

/* drop <i>, </font ...> and {\an8}-style tags; the overlay draws plain text */
static void strip_tags(char *s) {
    char *w = s;
    for (char *r = s; *r; r++) {
        if (*r == '<') { char *e = strchr(r, '>'); if (e) { r = e; continue; } }
        if (*r == '{' && r[1] == '\\') { char *e = strchr(r, '}'); if (e) { r = e; continue; } }
        *w++ = *r;
    }
    *w = 0;
}

static int load_srt(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
    char *buf = malloc(n + 1);
    if (!buf || fread(buf, 1, n, f) != (size_t)n) { fclose(f); free(buf); return -1; }
    fclose(f); buf[n] = 0;
    int cap = 0;
    char *line, *save = NULL, *text = NULL;
    double st = -1, en = -1;
    /* line by line: a "-->" line starts a cue, its text lines follow until the next "-->" line */
    for (line = strtok_r(buf, "\n", &save); ; line = strtok_r(NULL, "\n", &save)) {
        if (line) { size_t l = strlen(line); if (l && line[l - 1] == '\r') line[l - 1] = 0; }
        char *arrow = line ? strstr(line, "-->") : NULL;
        if (!line || arrow) {
            if (text && st >= 0) {
                if (g_ncues == cap) { cap = cap ? cap * 2 : 1024; g_cues = realloc(g_cues, cap * sizeof *g_cues); }
                /* a trailing number line belongs to the next cue */
                size_t l = strlen(text);
                while (l && (text[l - 1] == '\n')) text[--l] = 0;
                char *nl = strrchr(text, '\n');
                if (nl && strspn(nl + 1, "0123456789") == strlen(nl + 1)) *nl = 0;
                else if (!nl && strspn(text, "0123456789") == l) text[0] = 0;
                strip_tags(text);
                g_cues[g_ncues++] = (Cue){st, en, text};
                text = NULL;
            }
            free(text); text = NULL;
            if (!line) break;
            st = srt_time(line); en = srt_time(arrow + 3 + strspn(arrow + 3, " "));
            text = calloc(1, 1);
            continue;
        }
        if (!text) continue;
        size_t a = strlen(text), b = strlen(line);   /* strtok skips blank lines, so the next cue's number lands here */
        text = realloc(text, a + b + 2);
        if (a && text[a - 1] != '\n') strcat(text, "\n");
        strcat(text, line);
    }
    free(buf);
    return g_ncues;
}

static uint32_t utf8_next(const char **p) {
    const unsigned char *s = (const unsigned char *)*p;
    uint32_t c = *s++;
    int n = c >= 0xF0 ? 3 : c >= 0xE0 ? 2 : c >= 0xC0 ? 1 : 0;
    if (n) c &= 0x3F >> n;
    while (n-- && (*s & 0xC0) == 0x80) c = (c << 6) | (*s++ & 0x3F);
    *p = (const char *)s;
    return c;
}

static int advance_of(uint32_t c) {
    if (FT_Load_Char(g_face, c, FT_LOAD_DEFAULT)) return 0;
    return (int)(g_face->glyph->advance.x >> 6);
}

/* width in pixels of bytes [s, e) */
static int text_width(const char *s, const char *e) {
    int w = 0;
    while (s < e) w += advance_of(utf8_next(&s));
    return w;
}

static double wall(void) { struct timeval tv; gettimeofday(&tv, NULL); return tv.tv_sec + tv.tv_usec / 1e6; }

/* "over" blend of colour (ARGB, its alpha times coverage a) onto a non-premultiplied ARGB pixel */
static void blend(uint32_t *d, uint32_t col, int a) {
    a = a * (int)(col >> 24) / 255;
    if (a <= 0) return;
    uint32_t da = *d >> 24;
    uint32_t na = a + da * (255 - a) / 255;
    if (!na) return;
    uint32_t out = na << 24;
    for (int sh = 0; sh <= 16; sh += 8) {
        uint32_t sc = (col >> sh) & 0xFF, dc = (*d >> sh) & 0xFF;
        out |= ((sc * a + dc * da * (255 - a) / 255) / na) << sh;
    }
    *d = out;
}

static void fill(uint32_t *px, int pitch, int x0, int y0, int x1, int y1, uint32_t col) {
    if (x0 < 0) x0 = 0;
    if (y0 < 0) y0 = 0;
    if (x1 > SUB_W) x1 = SUB_W;
    if (y1 > SUB_H) y1 = SUB_H;
    for (int y = y0; y < y1; y++) {
        uint32_t *row = (uint32_t *)((uint8_t *)px + y * pitch);
        for (int x = x0; x < x1; x++) { if ((col >> 24) == 0xFF) row[x] = col; else blend(&row[x], col, 255); }
    }
}

/* outline: grow the glyph by `outline` px in black first (pass 0), then the glyph in col (pass 1) */
static void draw_glyph(uint32_t *px, int pitch, FT_Bitmap *bm, int x0, int y0, int outline, uint32_t col) {
    for (int y = 0; y < (int)bm->rows; y++)
        for (int x = 0; x < (int)bm->width; x++) {
            int a = bm->buffer[y * bm->pitch + x];
            if (!a) continue;
            for (int dy = -outline; dy <= outline; dy++)
                for (int dx = -outline; dx <= outline; dx++) {
                    if (outline && dx * dx + dy * dy > outline * outline + 1) continue;
                    int X = x0 + x + dx, Y = y0 + y + dy;
                    if (X < 0 || Y < 0 || X >= SUB_W || Y >= SUB_H) continue;
                    uint32_t *d = (uint32_t *)((uint8_t *)px + Y * pitch) + X;
                    if (outline) { if ((uint32_t)a > (*d >> 24)) *d = (uint32_t)a << 24; }   /* black, max alpha */
                    else blend(d, col, a);
                }
        }
}

/* text in [s, e) at the face's current size; align 0 left of x, 1 centred on x, 2 right of x */
static void draw_text(uint32_t *px, int pitch, const char *s, const char *e, int x, int baseline, int align, int outline, uint32_t col) {
    if (align) x -= text_width(s, e) / (align == 1 ? 2 : 1);
    for (int pass = outline ? 0 : 1; pass < 2; pass++) {
        int cx = x;
        for (const char *p = s; p < e;) {
            uint32_t c = utf8_next(&p);
            if (FT_Load_Char(g_face, c, FT_LOAD_RENDER)) continue;
            FT_GlyphSlot g = g_face->glyph;
            draw_glyph(px, pitch, &g->bitmap, cx + g->bitmap_left, baseline - g->bitmap_top, pass ? 0 : outline, col);
            cx += (int)(g->advance.x >> 6);
        }
    }
}

/* cut s so that it fits w pixels, ending in "..." */
static void fit(char *s, int w) {
    if (text_width(s, s + strlen(s)) <= w) return;
    int dots = text_width("...", "..." + 3);
    size_t n = strlen(s);
    while (n && text_width(s, s + n) + dots > w) { n--; while (n && ((unsigned char)s[n] & 0xC0) == 0x80) n--; }
    strcpy(s + n, "...");
}

#define MAX_LINES 8
/* split text into lines that fit: at '\n', and wrap at spaces */
static int layout(const char *t, const char **ls, const char **le) {
    int n = 0;
    const char *p = t;
    while (*p && n < MAX_LINES) {
        const char *end = strchr(p, '\n'); if (!end) end = p + strlen(p);
        const char *start = p;
        while (start < end && n < MAX_LINES) {
            const char *cut = end;
            if (text_width(start, end) > SUB_MAXW) {          /* last space that still fits */
                const char *best = NULL;
                for (const char *q = start; q < end; q++)
                    if (*q == ' ') { if (text_width(start, q) <= SUB_MAXW) best = q; else break; }
                if (best) cut = best;
            }
            ls[n] = start; le[n] = cut; n++;
            start = cut < end ? cut + 1 : end;
        }
        p = *end ? end + 1 : end;
    }
    return n;
}

/* --- the bar --- */
static double g_osd_until;        /* wall time it hides (0 = hidden, <0 = until told otherwise) */
static char g_osd_title[256], g_osd_hint[256];
static double g_osd_dur;          /* item length in seconds (0 = live: no progress) */
static double g_osd_pos = -1;     /* position to show instead of the playing one (seek target, paused) */
static double g_item_t = -1;      /* the playing position (item seconds), from sljf-play */
static long g_osd_drawn = -1;     /* second of the clock last drawn */
static double g_osd_left;         /* while the static is up the bar's time stands still: seconds it still has */

static int osd_visible(void) { return g_osd_until < 0 || (g_osd_until > 0 && wall() < g_osd_until); }

static void hms(char *b, size_t n, double t) {
    long s = t < 0 ? 0 : (long)t;
    if (s >= 3600) snprintf(b, n, "%ld:%02ld:%02ld", s / 3600, s / 60 % 60, s % 60);
    else snprintf(b, n, "%ld:%02ld", s / 60, s % 60);
}

static void draw_osd(uint32_t *px, int pitch) {
    int y0 = SUB_H - OSD_H;
    fill(px, pitch, 0, y0, SUB_W, SUB_H, C_BG);
    double pos = g_osd_pos >= 0 ? g_osd_pos : g_item_t;
    char title[256]; snprintf(title, sizeof title, "%s", g_osd_title);
    char hint[256]; snprintf(hint, sizeof hint, "%s", g_osd_hint);
    FT_Set_Pixel_Sizes(g_face, 0, 32);
    fit(title, SUB_W - 80);
    draw_text(px, pitch, title, title + strlen(title), 40, y0 + 44, 0, 0, C_FG);
    FT_Set_Pixel_Sizes(g_face, 0, 26);
    char a[32], b[40];
    int ty = y0 + 106;                                /* times row; the hint (or the length) in its middle */
    if (g_osd_dur > 0) {
        int bx0 = 40, bx1 = SUB_W - 40, by = y0 + 64;
        double f = pos / g_osd_dur; if (f < 0) f = 0; if (f > 1) f = 1;
        fill(px, pitch, bx0, by, bx1, by + 8, C_TRACK);
        fill(px, pitch, bx0, by, bx0 + (int)((bx1 - bx0) * f), by + 8, C_ACC);
        hms(a, sizeof a, pos);
        char r[32]; hms(r, sizeof r, g_osd_dur - pos); snprintf(b, sizeof b, "-%s", r);
        draw_text(px, pitch, a, a + strlen(a), bx0, ty, 0, 0, C_DIM);
        draw_text(px, pitch, b, b + strlen(b), bx1, ty, 2, 0, C_DIM);
        if (*hint) { fit(hint, SUB_W - 2 * 160); draw_text(px, pitch, hint, hint + strlen(hint), SUB_W / 2, ty, 1, 0, C_ACC); }
        else { hms(a, sizeof a, g_osd_dur); draw_text(px, pitch, a, a + strlen(a), SUB_W / 2, ty, 1, 0, C_DIM); }
    } else {
        ty = y0 + 96;
        draw_text(px, pitch, "LIVE", "LIVE" + 4, 40, ty, 0, 0, C_ACC);
        if (*hint) { fit(hint, SUB_W - 2 * 160); draw_text(px, pitch, hint, hint + strlen(hint), SUB_W / 2, ty, 1, 0, C_ACC); }
    }
    g_osd_drawn = (long)pos;
}

static int draw_init(void) {
    if (g_draw) return g_draw > 0 ? 0 : -1;
    g_draw = -1;
    const char *font = getenv("SLJF_SUB_FONT");
    if (!font) font = "/mnt/scratch/sljf/sub.ttf";
    if (FT_Init_FreeType(&g_ft) || FT_New_Face(g_ft, font, 0, &g_face)) {
        if (FT_New_Face(g_ft, "/usr/share/fonts/NotoSans-Regular.ttf", 0, &g_face)) { printf("subs: no font\n"); return -1; }
    }
    for (int i = 0; i < 2; i++) {
        g_ov[i] = SLVideo_CreateOverlay(g_vctx, SUB_W, SUB_H);
        if (!g_ov[i]) { printf("subs: SLVideo_CreateOverlay failed\n"); return -1; }
        SLVideo_SetOverlayDisplayArea(g_ov[i], 0.0f, 1.0f - (float)SUB_H / 720, 1.0f, (float)SUB_H / 720);
    }
    if (!g_cur) g_cur = strdup("");
    g_draw = 1;
    return 0;
}

/* draw the strip: the bar (when showing) and the subtitle text above it */
static void redraw(void) {
    if (g_suspended || !g_vctx || draw_init()) return;   /* !g_vctx: an osd line before subs_init (--static wait) */
    int osd = osd_visible();
    const char *t = g_cur ? g_cur : "";
    if (!*t && !osd) {
        if (g_shown) { SLVideo_HideOverlay(g_ov[g_ovi]); g_shown = 0; }
        return;
    }
    g_ovi ^= 1;
    uint32_t *px; int pitch;
    SLVideo_GetOverlayPixels(g_ov[g_ovi], &px, &pitch);
    if (!px) return;
    memset(px, 0, (size_t)pitch * SUB_H);
    if (osd) draw_osd(px, pitch);
    if (*t) {
        FT_Set_Pixel_Sizes(g_face, 0, SUB_PX);
        const char *ls[MAX_LINES], *le[MAX_LINES];
        int n = layout(t, ls, le);
        int bottom = osd ? SUB_H - OSD_H - 14 : SUB_H - SUB_MARGIN;
        int first = bottom - (n - 1) * SUB_LINE, dropped = 0;
        while (n && first < SUB_PX) { first += SUB_LINE; n--; dropped++; }   /* too many lines: keep the last ones */
        for (int i = 0; i < n; i++)
            draw_text(px, pitch, ls[i + dropped], le[i + dropped], SUB_W / 2, first + i * SUB_LINE, 1, SUB_OUTLINE, 0xFFFFFFFFu);
    }
    SLVideo_ShowOverlay(g_ov[g_ovi]);
    g_shown = 1;
}

/* SLJF_SUBS (srt file), SLJF_SUB_FONT (ttf). Returns 0 when subtitles are on (the file may come later).
   Always keeps the video context: the bar needs it too. */
int subs_init(CSLVideoContext *vctx) {
    g_vctx = vctx;
    const char *path = getenv("SLJF_SUBS");
    if (!path || !*path) return -1;
    g_path = path;
    return 0;
}

static int subs_load(void) {
    if (load_srt(g_path) <= 0) { printf("subs: no cues in %s\n", g_path); return -1; }
    if (draw_init()) return -1;
    printf("subs: %d cues from %s\n", g_ncues, g_path);
    g_ready = 1;
    return 0;
}

/* t = position in the item (seconds) of the picture on screen now */
void subs_update(double t) {
    if (!g_ready) {
        struct stat st;
        if (!g_path || time(NULL) == g_looked) return;
        g_looked = time(NULL);
        if (stat(g_path, &st)) return;                  /* not downloaded yet */
        if (subs_load()) { g_path = NULL; return; }     /* there but unusable: stop looking */
    }
    if (!g_ov[0]) return;
    static int from;
    if (from >= g_ncues || (from > 0 && g_cues[from - 1].end > t + 30)) from = 0;   /* moved back: rescan */
    while (from < g_ncues && g_cues[from].end < t - 30) from++;
    char text[2048]; text[0] = 0;
    for (int i = from; i < g_ncues && g_cues[i].start <= t; i++)
        if (g_cues[i].end > t && *g_cues[i].text) {
            if (*text) strncat(text, "\n", sizeof text - strlen(text) - 1);
            strncat(text, g_cues[i].text, sizeof text - strlen(text) - 1);
        }
    if (!strcmp(text, g_cur)) return;
    free(g_cur); g_cur = strdup(text);
    char *nl = strchr(text, '\n'); if (nl) *nl = 0;
    printf("sub @%.1f %s\n", t, text);              /* sljfd ignores these; sljf-test shows them */
    redraw();
}

/* One line from sljfd: "osd SECS\tTITLE\tDURATION\tHINT\tPOS"
     SECS  how long the bar stays (0 = hide now, -1 = until the next osd line)
     DURATION  item length in seconds (0 = live), POS  position to show (-1 = the playing one) */
void osd_command(const char *line) {
    if (strncmp(line, "osd ", 4)) return;
    char buf[1024]; snprintf(buf, sizeof buf, "%s", line + 4);
    char *f[5] = {0}; int n = 0;
    for (char *p = buf; n < 5; n++) { f[n] = p; char *tab = strchr(p, '\t'); if (!tab) { n++; break; } *tab = 0; p = tab + 1; }
    double secs = atof(f[0]);
    if (n > 1) snprintf(g_osd_title, sizeof g_osd_title, "%s", f[1]);
    g_osd_dur = n > 2 ? atof(f[2]) : 0;
    snprintf(g_osd_hint, sizeof g_osd_hint, "%s", n > 3 ? f[3] : "");
    g_osd_pos = n > 4 ? atof(f[4]) : -1;
    g_osd_until = secs < 0 ? -1 : secs > 0 ? wall() + secs : 0;
    if (g_suspended) g_osd_left = secs > 0 ? secs : 0;   /* the channel banner sent while tuning: shown after it */
    redraw();
}

/* every pass of the player's loop: t = playing position (item seconds, <0 unknown); redraws the bar when its clock
   moves to another second and hides it when its time is up */
void osd_tick(double t) {
    if (t >= 0) g_item_t = t;
    if (!g_osd_until || g_suspended) return;
    if (!osd_visible()) { g_osd_until = 0; redraw(); return; }
    double pos = g_osd_pos >= 0 ? g_osd_pos : g_item_t;
    if ((long)pos != g_osd_drawn) redraw();
}

/* on = 1: hide the strip and draw nothing (the static is up; the Link shows one overlay at a time); on = 0: draw it
   again as it is now (cues and the bar kept their state meanwhile; the bar's timer is paused, so the channel banner
   sent while a channel tunes behind the static shows for its full time once the picture is up) */
void subs_suspend(int on) {
    if (on == g_suspended) return;
    g_suspended = on;
    if (on) {
        g_osd_left = g_osd_until > 0 ? g_osd_until - wall() : 0;
        if (g_osd_left < 0) g_osd_left = 0;
        if (g_shown) { SLVideo_HideOverlay(g_ov[g_ovi]); g_shown = 0; }
    } else {
        if (g_osd_left > 0) g_osd_until = wall() + g_osd_left;   /* the bar gets the time it had left */
        g_osd_left = 0;
        if (g_draw > 0 || osd_visible()) redraw();
    }
}

void subs_free(void) {
    for (int i = 0; i < 2; i++) if (g_ov[i]) { SLVideo_HideOverlay(g_ov[i]); SLVideo_FreeOverlay(g_ov[i]); g_ov[i] = NULL; }
    if (g_face) FT_Done_Face(g_face);
    if (g_ft) FT_Done_FreeType(g_ft);
}
