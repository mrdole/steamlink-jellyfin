/*
 * sljf-play: playback-core test for a Jellyfin client on the Valve Steam Link (option B).
 *
 * Plays one stream URL on the Steam Link's hardware decoder:
 *   - video: H.264 only, handed to SLVideo as Annex-B access units (the decoder paces and displays them)
 *   - audio: decoded on the CPU (AAC from Jellyfin), sent to SLAudio as 48 kHz s16 stereo
 * Jellyfin is asked (by sljfd, or tools/sljf-test for testing) to copy the H.264 video and convert only the audio to stereo AAC,
 * so the Link's single 1 GHz core only demuxes and decodes AAC.
 *
 * Usage:  sljf-play <url> [seconds]        (0 or no seconds = until the end)
 *         sljf-play --card                  no video: only the on-screen bar (sljfd shows it while paused)
 *         sljf-play --static [seconds]      Live TV tuning: TV static at once, then plays the URL of the first stdin
 *                                           line "url <URL>" (the static stays until its first picture). End of
 *                                           stdin or SIGTERM before the URL: exit 0. Hiss volume from SLJF_STATIC
 *                                           (unset = 0, silent static)
 *         sljf-play --channel [seconds]     a TV channel: like --static (the "url" line is item 1, [seconds] its
 *                                           limit), then a queue of items played one after the other into the same
 *                                           decoders (cutplan.h): "next v <limit_s> <start_s> <url>" a video (limit
 *                                           0 = to its end), "next card <secs> <text>" silent static with the text
 *                                           for secs. Prints "item N start" (its first picture on screen, or the card
 *                                           up), "item N end" (handed over or done), "queue empty" (the item is read
 *                                           to its end and nothing is queued). Ends once the last item has played
 *                                           out and stdin has ended. A stream error with a next item queued cuts to it.
 *   stdin: control lines from sljfd, "osd ..." = the on-screen bar (sljf-subs.c, osd_command), "url ..." (--static).
 *   End of stdin is ignored when playing (sljf-test gives none) and ends --card.
 *   SIGTERM stops it.  Exit code: 0 ended/stopped, 3 the stream broke.
 *   No pause: starving SLVideo does not pause it (its queue clock keeps running, tried 2026-11-19, the sync then
 *   threw away audio non-stop). sljfd pauses by stopping sljf-play and starting it again at the same position.
 *   env SLJF_HEADERS   extra HTTP headers ("Authorization: ...\r\n"), so the token is not in the URL
 *   env SLJF_UA        User-Agent (Cloudflare in front of the server blocks some defaults)
 *   env SLJF_CA        CA bundle for https (the Link's own list is from 2013); verification is on when set
 *   env SLJF_AV_OFFSET_MS  fine-tune lip sync: positive plays the sound later, negative earlier (default 0)
 *   env SLJF_START     where in the item the stream starts (seconds; Jellyfin's StartTimeTicks), for subtitle timing
 *   env SLJF_PREBUFFER_MS  hold this much stream before feeding the decoders (Live TV: it arrives at real time on
 *                      average but unevenly, so without a cushion every hiccup empties the decoders = stutter). The
 *                      reader then keeps reading up to 12 s ahead (live-buf.h). 0 = off (files, default)
 *   env SLJF_READAHEAD_S  how far the reader reads ahead (1-120 s, default 12; channels: sljfd sets 20 with a USB
 *                      stick, 60 without). Each item's FIFO also stops at 32 MB of packets.
 *   sljf-play --probe FILE  prints "probe ok <secs> <w>x<h> <vcodec> <acodec>" or "probe bad <why>" (no screen use)
 *   env SLJF_SUBS      SRT file: draw these subtitles on an SLVideo overlay (sljf-subs.c); SLJF_SUB_FONT the TTF
 *   env SLJF_STATIC    Live TV: show TV static (sljf-static.c) with a hiss at this volume (0-100 % of full scale,
 *                      0 = silent) while the stream opens and prebuffers and whenever the picture has stopped after
 *                      it started (video queue empty and no frame fed for 700 ms);
 *                      prints "static on" / "static off". Unset: no static
 *                      (films, episodes: as without this feature). Subtitles and the bar are not drawn under it.
 *   env SLJF_VOLUME    0-100 (default 100 = untouched): software volume, the Link itself always sends full-scale
 *                      audio over HDMI. stdin "volume N" changes it while playing (heard within the ~1 s queued).
 * Prints one status line per second: queued video ms, queued audio ms, packets, dropped, A/V offset, CPU time.
 * pos= is the time since the requested start: Jellyfin's stream with copied video begins at the keyframe before it
 * (about 3 s early) while the audio begins exactly there, so the first audio timestamp is the zero point.
 *
 * A/V sync: neither SLVideo nor SLAudio takes timestamps; each plays what it is given at its own pace. So the
 * player works out where each one is ("timestamp of the last thing submitted" minus "what is still queued") and
 * moves the audio to match the picture: silence is inserted when the sound is ahead, samples are dropped when it
 * is behind. Checked once a second, corrected when the average is off by more than SYNC_SLACK_MS.
 */
#ifndef SLJF_VERSION
#define SLJF_VERSION "dev"   /* scripts/build.sh passes the VERSION file */
#endif
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libavcodec/bsf.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libavutil/channel_layout.h>
#include <libswresample/swresample.h>
#include <SLVideo.h>
#include <SLAudio.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <pthread.h>
#include "sljf-static.h"
#include "live-buf.h"
#include "item.h"
#include "cutplan.h"

#define AUDIO_RATE 48000
#define AUDIO_CH 2
#define AUDIO_CHUNK_MS 20
#define AUDIO_CHUNK_SAMPLES (AUDIO_RATE * AUDIO_CHUNK_MS / 1000)
#define AUDIO_CHUNK_BYTES (AUDIO_CHUNK_SAMPLES * AUDIO_CH * 2)
#define AHEAD_MS 1000            /* keep about this much queued in each decoder */
#define GAP_AHEAD_MS 4000        /* a decoder stream made again, before its first picture (see room below) */
#define GAP_GIVEUP_S 10.0        /* ... and how long it may take to show one before its item is skipped */
#define SYNC_SLACK_MS 45         /* leave smaller A/V offsets alone */
#define SYNC_MAX_MS 3000         /* largest single correction */
#define V_NONE (-1e9)            /* v_first not known yet */

#if AUDIO_CHUNK_BYTES != STATIC_AUDIO_CHUNK_BYTES
#error "the static's audio stream is handed to the player: same chunk size"
#endif

int subs_init(CSLVideoContext *vctx);    /* sljf-subs.c */
void subs_update(double t);
void subs_free(void);
void osd_command(const char *line);
void osd_tick(double t);

static volatile int g_stop;
static char g_url[4096];                 /* from a "url <URL>" stdin line (--static) */
static void on_signal(int s) { (void)s; g_stop = 1; }

/* Volume: gain in 1/65536 steps on the s16 samples, squared from 0-100 so the steps sound even (50 = -12 dB,
   40 = -16 dB, 20 = -28 dB, 10 = -40 dB); 100 leaves the samples alone. One multiply + shift per sample. */
static int32_t g_gain = 65536;
static void set_volume(int v) {
    if (v < 0) v = 0;
    if (v > 100) v = 100;
    g_gain = (v * v * 65536 + 5000) / 10000;
    static_set_gain(g_gain);                            /* the static's hiss: same level relative to the show */
    printf("volume %d\n", v);
}
static void apply_gain(uint8_t *pcm, int bytes) {
    int32_t g = g_gain;
    if (g >= 65536) return;
    int16_t *p = (int16_t *)pcm;
    for (int i = 0, n = bytes / 2; i < n; i++) p[i] = (int16_t)((p[i] * g) >> 16);
}

/* Read-ahead FIFO (SLJF_PREBUFFER_MS, Live TV): packets read but not yet fed to the decoders, with their stream time.
   With a prebuffer a reader thread fills it, so a slow read never holds up feeding: Live TV at the live edge waits in
   av_read_frame for the network, and at times the server sends in bursts (Cartoon Network 2026-11-20: every read
   waited 230-490 ms, twice 2.9 s, for 3 min); when that wait was in the feeding loop both decoders ran dry with 3.8 s
   of packets in this FIFO (stutter, 60 sync corrections). Without a prebuffer (files) the loop reads inline.
   The FIFO and the reader belong to the item (item.h); the lock and the wake-up are shared. */
static double read_max;                               /* longest read since the status */
static double g_ahead;                                /* the reader stops while this much is read ahead */
static pthread_mutex_t q_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t q_cv = PTHREAD_COND_INITIALIZER;

static double now_s(void);
static int interrupt_cb(void *o) { struct item *it = o; return g_stop || (it && it->rd_quit); }   /* lets a blocked read end on stop */

#define FIFO_BYTES_MAX (32L * 1024 * 1024)            /* and this many packet bytes per item: 1080p holds 14-18 MB at 12 s, the Link has ~100 MB */

static double fifo_span(struct item *it) { return it->fifo_n ? it->last_ts - it->fifo_ts[it->fifo_head] : 0; }

/* one packet into the FIFO (q_mu held) */
static void fifo_push(struct item *it, AVPacket *pkt) {
    int64_t ts = pkt->pts != AV_NOPTS_VALUE ? pkt->pts : pkt->dts;
    if (ts != AV_NOPTS_VALUE) it->last_ts = ts * av_q2d(it->fmt->streams[pkt->stream_index]->time_base);
    if (!it->pushed++) it->first_ts = it->last_ts;
    it->bytes += pkt->size;
    AVPacket *q = av_packet_alloc();
    av_packet_move_ref(q, pkt);
    int at = (it->fifo_head + it->fifo_n) % FIFO_MAX;
    it->fifo[at] = q; it->fifo_ts[at] = it->last_ts; it->fifo_n++;
}

static void *reader(void *o) {
    struct item *it = o;
    AVPacket *pkt = av_packet_alloc();
    for (;;) {
        pthread_mutex_lock(&q_mu);
        while (!it->rd_quit && !g_stop && (it->fifo_n >= FIFO_MAX || it->bytes >= FIFO_BYTES_MAX || fifo_span(it) >= it->ahead)) {
            struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_nsec += 100000000; if (ts.tv_nsec >= 1000000000) { ts.tv_sec++; ts.tv_nsec -= 1000000000; }
            pthread_cond_timedwait(&q_cv, &q_mu, &ts);
        }
        int quit = it->rd_quit || g_stop;
        pthread_mutex_unlock(&q_mu);
        if (quit) break;
        double r0 = now_s();
        int err = av_read_frame(it->fmt, pkt);
        double w = now_s() - r0;
        pthread_mutex_lock(&q_mu);
        if (w > read_max) read_max = w;
        if (err < 0) {
            if (err == AVERROR_EOF) it->rd_eof = 1; else it->rd_err = err;
            pthread_mutex_unlock(&q_mu);
            break;
        }
        fifo_push(it, pkt);
        pthread_mutex_unlock(&q_mu);
    }
    av_packet_free(&pkt);
    return NULL;
}

int item_open(struct item *it, const char *url, AVDictionary *opts) {
    memset(it, 0, sizeof *it);
    it->vi = it->ai = -1;
    it->fmt = avformat_alloc_context();
    it->fmt->interrupt_callback.callback = interrupt_cb;   /* copied into the connection at open: set it first */
    it->fmt->interrupt_callback.opaque = it;
    AVDictionary *o = NULL;                           /* the open rewrites (and frees) the dictionary it is given: use a copy */
    av_dict_copy(&o, opts, 0);
    int err = avformat_open_input(&it->fmt, url, NULL, &o);
    av_dict_free(&o);
    if (err < 0) { fprintf(stderr, "open failed: %s\n", av_err2str(err)); return err; }   /* (fmt is freed by the failed open) */
    if ((err = avformat_find_stream_info(it->fmt, NULL)) < 0) { fprintf(stderr, "no stream info: %s\n", av_err2str(err)); return err; }
    return 0;
}

void item_close(struct item *it) {
    if (it->threaded) {                               /* the interrupt callback ends a read in progress */
        pthread_mutex_lock(&q_mu); it->rd_quit = 1; pthread_cond_broadcast(&q_cv); pthread_mutex_unlock(&q_mu);
        pthread_join(it->rd_th, NULL);
        it->threaded = 0;
    }
    while (it->fifo_n) { av_packet_free(&it->fifo[it->fifo_head]); it->fifo_head = (it->fifo_head + 1) % FIFO_MAX; it->fifo_n--; }
    it->bytes = 0;
    swr_free(&it->swr); avcodec_free_context(&it->adec); av_bsf_free(&it->bsf);
    avformat_close_input(&it->fmt);
}

static double now_s(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9;
}
static double cpu_s(void) {
    struct timespec t; clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t); return t.tv_sec + t.tv_nsec / 1e9;
}

/* --channel: the items queued after the one playing ("next" stdin lines), in arrival order */
#define QMAX 16
struct pend { int seq, card, dead; double limit, start; char arg[4096]; };   /* arg: the URL, or the card's text;
                                                       dead: its open failed (already reported, popped when it is next) */
static struct pend g_q[QMAX];
static int g_qh, g_qn, g_seq = 1;                    /* the "url" item is 1 */
static int g_channel;

static void queue_next(const char *s) {
    const char *rest = NULL; double a = 0, b = 0; int n = 0, card = 0;
    int seq = ++g_seq;                                /* every next line takes its number, dropped or not: sljfd counts them */
    if (!strncmp(s, "v ", 2) && sscanf(s + 2, "%lf %lf %n", &a, &b, &n) == 2 && n > 0) rest = s + 2 + n;
    else if (!strncmp(s, "card ", 5) && sscanf(s + 5, "%lf %n", &a, &n) == 1 && n > 0) { rest = s + 5 + n; card = 1; }
    if (!rest || (!card && !*rest)) { printf("next: not understood: %.60s\nitem %d end\n", s, seq); return; }
    if (g_qn == QMAX) { printf("next: queue full, dropped\nitem %d end\n", seq); return; }
    struct pend *p = &g_q[(g_qh + g_qn) % QMAX];
    p->seq = seq; p->card = card; p->dead = 0; p->limit = a; p->start = b;
    snprintf(p->arg, sizeof p->arg, "%s", rest);
    size_t l = strlen(p->arg); if (l && p->arg[l - 1] == '\r') p->arg[l - 1] = 0;   /* CRLF writer */
    g_qn++;
    printf("queued item %d: %s, %g s\n", p->seq, card ? "card" : "video", a);
}
static void queue_pop(void) { g_qh = (g_qh + 1) % QMAX; g_qn--; }

/* read whatever sljfd has written to stdin and act on complete lines; returns -1 once stdin has ended */
static int g_ctl_ended;
static int read_ctl(void) {
    static char buf[4096]; static int fill, set;
    if (g_ctl_ended) return -1;
    if (!set) { fcntl(0, F_SETFL, fcntl(0, F_GETFL) | O_NONBLOCK); set = 1; }
    for (;;) {
        ssize_t n = read(0, buf + fill, sizeof buf - 1 - fill);
        if (n == 0) { g_ctl_ended = 1; return -1; }
        if (n < 0) return errno == EAGAIN || errno == EWOULDBLOCK ? 0 : (g_ctl_ended = 1, -1);
        fill += (int)n; buf[fill] = 0;
        char *nl;
        while ((nl = strchr(buf, '\n'))) {
            *nl = 0;
            if (!strncmp(buf, "url ", 4)) {
                if (!g_url[0]) {
                    snprintf(g_url, sizeof g_url, "%s", buf + 4);
                    size_t l = strlen(g_url); if (l && g_url[l - 1] == '\r') g_url[l - 1] = 0;   /* CRLF writer */
                }
            }
            else if (!strncmp(buf, "volume ", 7)) set_volume(atoi(buf + 7));
            else if (g_channel && !strncmp(buf, "next ", 5)) queue_next(buf + 5);
            else osd_command(buf);
            fill -= (int)(nl + 1 - buf);
            memmove(buf, nl + 1, fill + 1);
        }
        if (fill >= (int)sizeof buf - 1) fill = 0;     /* a line too long: drop it */
    }
}

/* --channel: the new item's sound held at a cut until it can start in step with its picture (see main) */
#define HOLD_MAX 1024                                 /* AAC packets (~21 s) */
#define LEAD_GAP_S 0.25                               /* the most silence put before it (the rest is waited out) */
static AVPacket *g_hold[HOLD_MAX];
static int g_hold_n;
static void hold_clear(void) { while (g_hold_n) av_packet_free(&g_hold[--g_hold_n]); }
static void hold_push(AVPacket *pkt) {
    if (g_hold_n == HOLD_MAX) { av_packet_free(&g_hold[0]); memmove(g_hold, g_hold + 1, (HOLD_MAX - 1) * sizeof *g_hold); g_hold_n--; }
    AVPacket *q = av_packet_alloc();
    av_packet_move_ref(q, pkt);
    g_hold[g_hold_n++] = q;
}
/* the held sound back in front of the item's FIFO, less what starts before stream time `from`; returns packets kept */
static int hold_release(struct item *it, double from) {
    AVRational tb = it->fmt->streams[it->ai]->time_base;
    int kept = 0;
    pthread_mutex_lock(&q_mu);
    for (int i = g_hold_n - 1; i >= 0; i--) {         /* back to front: each goes in at the FIFO's head */
        AVPacket *q = g_hold[i];
        double ts = q->pts != AV_NOPTS_VALUE ? q->pts * av_q2d(tb) : from;
        if (ts + (q->duration > 0 ? q->duration * av_q2d(tb) : 0) <= from || it->fifo_n >= FIFO_MAX) { av_packet_free(&q); continue; }
        it->fifo_head = (it->fifo_head + FIFO_MAX - 1) % FIFO_MAX;
        it->fifo[it->fifo_head] = q; it->fifo_ts[it->fifo_head] = ts; it->fifo_n++; it->bytes += q->size;
        kept++;
    }
    g_hold_n = 0;
    pthread_mutex_unlock(&q_mu);
    return kept;
}

/* the item's streams: video (H.264 only; MP4/MKV get the Annex-B filter), audio and its decoder. <0 = unusable,
   already reported */
static int item_streams(struct item *it) {
    it->vi = av_find_best_stream(it->fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    it->ai = av_find_best_stream(it->fmt, AVMEDIA_TYPE_AUDIO, -1, -1, NULL, 0);
    if (it->vi < 0) { fprintf(stderr, "no video stream\n"); return -1; }
    /* Jellyfin's audio can start late in the stream (it converts audio while copying video), so probing may see an
       audio stream without parameters and av_find_best_stream skips it: take it anyway, the decoder works them out. */
    for (unsigned i = 0; it->ai < 0 && i < it->fmt->nb_streams; i++)
        if (it->fmt->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) it->ai = (int)i;
    AVStream *vs = it->fmt->streams[it->vi];
    if (vs->codecpar->codec_id != AV_CODEC_ID_H264) {
        fprintf(stderr, "video is %s, the Steam Link decoder only takes h264\n", avcodec_get_name(vs->codecpar->codec_id));
        return -1;
    }
    it->w = vs->codecpar->width; it->h = vs->codecpar->height;
    printf("video h264 %dx%d profile %d, %.3f fps\n", it->w, it->h, vs->codecpar->profile, av_q2d(vs->avg_frame_rate));
    /* MP4/MKV carry length-prefixed NALs: convert to Annex-B (MPEG-TS already is). */
    if (vs->codecpar->extradata_size && vs->codecpar->extradata[0] == 1) {
        const AVBitStreamFilter *f = av_bsf_get_by_name("h264_mp4toannexb");
        av_bsf_alloc(f, &it->bsf);
        avcodec_parameters_copy(it->bsf->par_in, vs->codecpar);
        it->bsf->time_base_in = vs->time_base;
        av_bsf_init(it->bsf);
        printf("converting to Annex-B\n");
    }
    if (it->ai >= 0) {
        AVStream *as = it->fmt->streams[it->ai];
        const AVCodec *c = avcodec_find_decoder(as->codecpar->codec_id);
        if (!c) { fprintf(stderr, "no decoder for audio %s, playing without sound\n", avcodec_get_name(as->codecpar->codec_id)); it->ai = -1; }
        else {
            it->adec = avcodec_alloc_context3(c);
            avcodec_parameters_to_context(it->adec, as->codecpar);
            if (avcodec_open2(it->adec, c, NULL) < 0) { fprintf(stderr, "audio decoder open failed\n"); return -1; }
        }
    }
    return 0;
}

static AVRational item_rate(struct item *it) {
    AVStream *vs = it->fmt->streams[it->vi];
    return vs->avg_frame_rate.num ? vs->avg_frame_rate : vs->r_frame_rate;
}

/* an SLVideo decoder stream set up for this item's video */
static CSLVideoStream *video_stream_new(CSLVideoContext *vctx, struct item *it) {
    CSLVideoStream *vstr = SLVideo_CreateStream(vctx, k_ESLVideoFormatH264, 0);   /* 0: B-frames allowed, queued */
    if (!vstr) { fprintf(stderr, "SLVideo_CreateStream failed\n"); return NULL; }
    static_tick(now_s());                               /* the static keeps moving through the setup (no-op without) */
    SLVideo_SetStreamVideoTransferMatrix(vstr, k_ESLVideoTransferMatrix_Automatic);
    AVRational fr = item_rate(it);
    if (fr.num) SLVideo_SetStreamTargetFramerate(vstr, fr.num, fr.den);
    if (it->fmt->bit_rate > 0) SLVideo_SetStreamTargetBitrate(vstr, (int)(it->fmt->bit_rate / 1000));
    return vstr;
}

static int stream_has_picture(CSLVideoStream *vstr) {
    int w = 0, h = 0; SLVideo_GetStreamResolution(vstr, &w, &h);
    return w > 0;
}

/* the programme's SLAudio stream: the static's own, if it made one (the Link has one output: no second stream) */
static int audio_stream_open(CSLAudioContext **actx, CSLAudioStream **astr) {
    static_give_audio(actx, astr);
    if (!*astr) {
        *actx = SLAudio_CreateContext();
        *astr = *actx ? SLAudio_CreateStream(*actx, AUDIO_RATE, AUDIO_CH, AUDIO_CHUNK_BYTES, 0) : NULL;
    }
    if (!*astr) { fprintf(stderr, "SLAudio stream failed\n"); return -1; }
    static_give_audio(actx, astr);                      /* rebuffer hiss goes into the programme stream */
    return 0;
}

/* --channel: the next queued video (the first after any cards) is opened in the background (its own demuxer, reader
   thread and FIFO) when the one playing is nearly read (cutplan.h), so the cut finds its first packets waiting. While
   it waits its reader keeps only NEXT_AHEAD_S read ahead (two full FIFOs are too much for the Link's ~100 MB). */
#define NEXT_AHEAD_S 4.0
enum { NX_NONE, NX_OPENING, NX_READY };
static int g_nx;                                      /* state of the next item */
static int g_op_idx;                                  /* its slot in g_q (stays put until its cut) */
static AVDictionary *g_opts;                          /* HTTP options for every open */
static pthread_t g_op_th;
static volatile int g_op_res;                         /* the opener's result: 0 running, 1 ok, -1 failed */
static double g_op_t0;
static int g_broke;                                   /* an item broke or failed to open and nothing has started since */

static void *opener(void *o) {
    struct item *it = o;
    int err = item_open(it, g_q[g_op_idx].arg, g_opts);
    if (err >= 0) err = item_streams(it);
    g_op_res = err < 0 ? -1 : 1;
    return NULL;
}

/* seconds until the item's cut: its limit (or length) less what has been fed of it (the FIFO head's stream time;
   the limit is checked there too), and no more than what its FIFO holds once the reader is at the end (an item
   shorter than it said); 1e9 unknown (q_mu held) */
static double item_left(struct item *it) {
    double fed = !it->pushed ? 0 : (it->fifo_n ? it->fifo_ts[it->fifo_head] : it->last_ts) - it->first_ts;
    double left = 1e9;
    if (it->limit_s > 0) left = it->limit_s - fed;
    else if (it->fmt && it->fmt->duration != AV_NOPTS_VALUE && it->fmt->duration > 0)
        left = it->fmt->duration / (double)AV_TIME_BASE - fed;
    if (it->rd_eof && fifo_span(it) < left) left = fifo_span(it);
    return left;
}

/* an item that will not play: reported now (sljfd moves on), dropped from the queue when its turn comes */
static void item_failed(struct pend *p, const char *why) {
    printf("item %d: %s, skipped\nitem %d end\n", p->seq, why, p->seq);
    p->dead = 1;
    g_broke = 1;
}

/* start opening the first queued video when it is time; take it once opened. card_left >= 0: a card is on (cur holds
   nothing) with that many seconds left */
static void chan_open(struct item *cur, struct item *nxt, double card_left) {
    if (g_nx == NX_OPENING) {
        if (!g_op_res) return;
        pthread_join(g_op_th, NULL);
        struct pend *p = &g_q[g_op_idx];
        g_nx = NX_NONE;
        if (g_op_res > 0) {
            nxt->seq = p->seq; nxt->kind = 0; nxt->limit_s = p->limit; nxt->start_s = p->start;
            nxt->ahead = NEXT_AHEAD_S;
            if (pthread_create(&nxt->rd_th, NULL, reader, nxt) == 0) {
                nxt->threaded = 1;
                printf("item %d opened in %.1f s\n", p->seq, now_s() - g_op_t0);
                g_nx = NX_READY;
                return;
            }
            printf("reader thread failed\n");
        }
        item_close(nxt);
        item_failed(p, "open failed");
        return;
    }
    if (g_nx != NX_NONE) return;
    int idx = -1;                                     /* the first live video in the queue (cards need no opening) */
    for (int i = 0; i < g_qn && idx < 0; i++) {
        struct pend *p = &g_q[(g_qh + i) % QMAX];
        if (!p->card && !p->dead) idx = (g_qh + i) % QMAX;
    }
    if (idx < 0) return;
    double left = card_left; int end = 1;
    if (card_left < 0) {
        pthread_mutex_lock(&q_mu);
        /* (the reader at the end of a stream with a big read-ahead still has up to a minute to play: item_left
           counts that, so the next item's connection is not opened early and left idle) */
        left = item_left(cur); end = cur->rd_err || cur->over;
        pthread_mutex_unlock(&q_mu);
    }
    if (!item_should_open_next(left, end, 1, 0)) return;
    g_op_idx = idx;
    printf("item %d: opening\n", g_q[idx].seq);
    g_op_res = 0; g_op_t0 = now_s();
    if (pthread_create(&g_op_th, NULL, opener, nxt) == 0) { g_nx = NX_OPENING; return; }
    item_failed(&g_q[idx], "open failed (no thread)");
}

/* --card: the bar on its own (paused: sljf-play is not running then, see the top) until SIGTERM or end of stdin */
static int card(void) {
    CSLVideoContext *vctx = SLVideo_CreateContext();
    if (!vctx) { fprintf(stderr, "SLVideo_CreateContext failed\n"); return 1; }
    subs_init(vctx);
    printf("card\n");
    while (!g_stop && read_ctl() >= 0) { osd_tick(-1); usleep(50000); }
    subs_free();
    SLVideo_FreeContext(vctx);
    return 0;
}

/* --probe FILE: one line for sljfd (daemon: probe_line), no SLVideo/SLAudio, so it runs while another player owns the
   screen. "probe ok <secs> <w>x<h> <vcodec> <acodec>" when the Link can play it (h264 8-bit up to 1920x1088, sound
   aac/mp3/ac3 or none), else "probe bad <why>". Exit 0 either way (2 without a file). */
static int probe(const char *path) {
    av_log_set_level(AV_LOG_QUIET);
    AVFormatContext *fmt = NULL;
    AVDictionary *o = NULL;
    av_dict_set(&o, "rw_timeout", "15000000", 0);
    int err = avformat_open_input(&fmt, path, NULL, &o);
    av_dict_free(&o);
    if (err < 0) { printf("probe bad open: %s\n", av_err2str(err)); return 0; }
    if ((err = avformat_find_stream_info(fmt, NULL)) < 0) { printf("probe bad no stream info: %s\n", av_err2str(err)); avformat_close_input(&fmt); return 0; }
    int vi = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    int ai = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, NULL, 0);
    double secs = fmt->duration > 0 ? fmt->duration / (double)AV_TIME_BASE : 0;
    if (vi < 0) { puts("probe bad no video"); avformat_close_input(&fmt); return 0; }
    AVCodecParameters *vp = fmt->streams[vi]->codecpar;
    const char *vn = avcodec_get_name(vp->codec_id), *an = "none";
    const AVPixFmtDescriptor *pd = vp->format >= 0 ? av_pix_fmt_desc_get((enum AVPixelFormat)vp->format) : NULL;
    int deep = vp->bits_per_raw_sample > 8 || (pd && pd->comp[0].depth > 8) ||
               (vp->codec_id == AV_CODEC_ID_H264 && (vp->profile == 110 || vp->profile == 122 || vp->profile == 244));
    if (ai >= 0) an = avcodec_get_name(fmt->streams[ai]->codecpar->codec_id);
    if (!(secs > 0)) puts("probe bad no duration");
    else if (vp->codec_id != AV_CODEC_ID_H264) printf("probe bad video %s\n", vn);
    else if (deep) puts("probe bad h264 not 8-bit");
    else if (vp->width <= 0 || vp->height <= 0 || vp->width > 1920 || vp->height > 1088) printf("probe bad size %dx%d\n", vp->width, vp->height);
    else if (ai >= 0 && strcmp(an, "aac") && strcmp(an, "mp3") && strcmp(an, "ac3")) printf("probe bad audio %s\n", an);
    else printf("probe ok %.3f %dx%d %s %s\n", secs, vp->width, vp->height, vn, an);
    avformat_close_input(&fmt);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "--version")) { puts(SLJF_VERSION); return 0; }
    if (argc < 2) { fprintf(stderr, "usage: sljf-play <url> [seconds] | --static [seconds] | --channel [seconds] | --card | --probe <file> | --version\n"); return 2; }
    if (!strcmp(argv[1], "--probe")) {
        if (argc < 3) { fprintf(stderr, "usage: sljf-play --probe <file>\n"); return 2; }
        return probe(argv[2]);
    }
    if (!strcmp(argv[1], "--card")) {
        signal(SIGINT, on_signal); signal(SIGTERM, on_signal); setvbuf(stdout, NULL, _IOLBF, 0);
        return card();
    }
    const char *url = argv[1];
    double limit = argc > 2 ? atof(argv[2]) : 0;
    signal(SIGINT, on_signal); signal(SIGTERM, on_signal);
    setvbuf(stdout, NULL, _IOLBF, 0);                   /* status lines show up over ssh without a tty */
    if (getenv("SLJF_DEBUG")) av_log_set_level(AV_LOG_DEBUG);
    if (getenv("SLJF_VOLUME") && *getenv("SLJF_VOLUME")) set_volume(atoi(getenv("SLJF_VOLUME")));

    /* TV static (Live TV): up before anything slow (the URL wait, the network), so the screen is never black long */
    g_channel = !strcmp(argv[1], "--channel");
    int wait_url = g_channel || !strcmp(argv[1], "--static");
    int svol = static_volume();
    if (wait_url && svol < 0) svol = 0;
    CSLVideoContext *vctx = NULL;
    if (svol >= 0) {
        atexit(static_free);                            /* error returns: the overlay and an own audio stream */
        vctx = SLVideo_CreateContext();
        if (!vctx) { fprintf(stderr, "SLVideo_CreateContext failed\n"); return 1; }
        static_show(vctx, svol);
    }
    if (wait_url) {                                     /* sljfd writes the URL once the server opened the channel */
        while (!g_stop && !g_url[0] && read_ctl() >= 0) { static_tick(now_s()); usleep(10000); }
        if (!g_url[0]) { static_free(); SLVideo_FreeContext(vctx); printf("static: no url, stopped\n"); return 0; }
        url = g_url;
    }

    AVDictionary *opts = NULL;
    const char *h = getenv("SLJF_HEADERS"), *ua = getenv("SLJF_UA");
    if (h) av_dict_set(&opts, "headers", h, 0);
    av_dict_set(&opts, "user_agent", ua ? ua : "SteamLinkJellyfin/" SLJF_VERSION, 0);
    const char *ca = getenv("SLJF_CA");                /* current CA list shipped with the app */
    if (ca) { av_dict_set(&opts, "ca_file", ca, 0); av_dict_set(&opts, "tls_verify", "1", 0); }
    av_dict_set(&opts, "rw_timeout", "15000000", 0);  /* 15 s: give up instead of hanging */
    /* no "reconnect": Jellyfin's stream cannot be resumed by byte offset. On a dropped connection sljf-play exits
       with code 3 and sljfd starts it again at the last position. */

    /* the item playing and (--channel) the next one, in turn; the reader threads hold pointers to them */
    static struct item items[2];
    struct item *cur = &items[0], *nxt = &items[1];
    double t0 = now_s();
    static_background(1);                               /* the static keeps moving while these block */
    int err = item_open(cur, url, opts);
    static_background(0);
    if (g_channel) g_opts = opts;                       /* every next item opens with the same options */
    else av_dict_free(&opts);
    if (err < 0) return g_stop && svol >= 0 ? 0 : 1;
    printf("opened in %.1f s, format %s\n", now_s() - t0, cur->fmt->iformat->name);
    if (item_streams(cur) < 0) return 1;
    cur->seq = 1;
    cur->start_s = getenv("SLJF_START") ? atof(getenv("SLJF_START")) : 0;
    if (g_channel) { cur->limit_s = limit; limit = 0; }   /* channel: the item's limit, in stream time */
    AVStream *vs = cur->fmt->streams[cur->vi];

    if (!vctx) vctx = SLVideo_CreateContext();         /* (made before the open when the static is on) */
    if (!vctx) { fprintf(stderr, "SLVideo_CreateContext failed\n"); return 1; }
    int dw = 0, dh = 0; SLVideo_GetDisplayResolution(vctx, &dw, &dh);
    printf("display %dx%d\n", dw, dh);
    CSLVideoStream *vstr = video_stream_new(vctx, cur);
    if (!vstr) return 1;
    int vw = cur->w, vh = cur->h;                     /* the size vstr decodes */
    AVRational fr = item_rate(cur);
    int subs = subs_init(vctx) == 0;

    CSLAudioContext *actx = NULL; CSLAudioStream *astr = NULL;
    if (cur->adec) {
        if (audio_stream_open(&actx, &astr) < 0) return 1;
        printf("audio %s -> s16 stereo 48 kHz\n", cur->adec->codec->name);
    }
    static_tick(now_s());

    AVPacket *pkt = av_packet_alloc(), *fpkt = av_packet_alloc();
    AVFrame *afr = av_frame_alloc();
    static uint8_t abuf[AUDIO_CHUNK_BYTES * 64];
    int afill = 0;                                    /* bytes of pending PCM in abuf */
    long vframes = 0, vfail = 0, apk = 0;
    double fdur = fr.num ? av_q2d(av_inv_q(fr)) : 1.0 / 24;   /* seconds per video frame */
    /* The frame rate the stream says at open can be wrong (Live TV: ErsatzTV channel probed 25, sent 29.97, 2026-11-20:
       SLVideo paced at the wrong speed and the sync, counting frames x fdur, dropped sound non-stop). So it is measured
       from the packets' dts every 60 frames and the decoder's target rate follows. Video seconds fed = v_acc (frames
       before the last change) + (vframes - n_base) * fdur. */
    double v_acc = 0; long n_base = 0;
    double dts_ring[60]; int dts_n = 0;
    /* pts (s) of the first video frame less the video fed before it (channel: earlier items, so it can be below 0;
       V_NONE = not seen yet) / end of decoded audio */
    double v_first = V_NONE, a_end = -1;
    double a_first = -1;                              /* pts (s) of the first audio packet = the requested start */
    double sub_clock = -1, sub_wall = 0;              /* subtitle clock (item seconds) and when it was last set */
    int started = 0, low_logged = 0;
    double prebuf = getenv("SLJF_PREBUFFER_MS") ? atof(getenv("SLJF_PREBUFFER_MS")) / 1000.0 : 0;
    double fifo_low = 1e9;                            /* smallest read-ahead since the last status line */
    g_ahead = livebuf_ahead(prebuf);
    cur->ahead = g_ahead;
    if (prebuf > 0 || g_channel) {                    /* channel: always a reader, the next item opens beside it */
        if (pthread_create(&cur->rd_th, NULL, reader, cur) == 0) cur->threaded = 1;
        else printf("reader thread failed, reading inline\n");
    }
    double sync_sum = 0, sync_last = 0, sync_fix_t = 0; int sync_n = 0, sync_fixes = 0;
    double sync_hold = 0;                             /* no sync measurement before this (3 s after a cut) */
    long adrop = 0;                                   /* bytes of PCM still to throw away (audio was late) */
    double av_off = getenv("SLJF_AV_OFFSET_MS") ? atof(getenv("SLJF_AV_OFFSET_MS")) / 1000.0 : 0;
    double start = now_s(), last = start, cpu0 = cpu_s();
    int eof = 0, rc = 0;
    double eof_wait = -1;                             /* when to stop waiting for the decoders after the end */
    int st_play = 0; long st_frames = 0;              /* picture started; frames fed when the static went up */
    struct picwatch pw; picwatch_reset(&pw, 0, start);
    /* --channel: the cut state */
    double card_end = -1;                             /* a card is on until then (cur holds no stream) */
    double drain_until = -1;                          /* a reset / card waits for the old picture to play out */
    double gap_t0 = -1;                               /* the decoder stream was made again then: no picture yet */
    double it_vfed0 = 0;                              /* video seconds fed before this item */
    int it_started = 0, it_eof_said = 0, it_err_said = 0, q_empty_said = 0;
    int force_reset = 0;                              /* the decoder stream showed nothing: the next cut makes a new one */
    double it_start_t = 0;                            /* when this item's first picture showed */
    int a_hold = 0;                                   /* the new item's sound is held after a cut (1 same stream, 2 new
                                                         stream: not before its first picture) until in step, see below */

    if (g_channel && cur->adec) a_hold = 2;           /* channel: item 1's sound starts in step too (its lead-in) */
    while (!g_stop) {
        double t = now_s();
        if (limit > 0 && t - start > limit) break;
        double vfed = v_acc + (vframes - n_base) * fdur;     /* seconds of video handed to the decoder */
        uint32_t vq = SLVideo_GetQueuedVideoMS(vstr);
        uint32_t aq = astr ? SLAudio_GetQueuedAudioSamples(astr) * 1000 / AUDIO_RATE : 0;
        /* TV static: off once the first frame is on screen; on again when the picture has stopped (video queue
           empty and no frame fed for 700 ms: the queue reads 0 while frames come in just in time, which is moving
           picture and flashed the static every few seconds, 2026-11-21), off when frames come in again */
        int was_play = st_play;
        if (!st_play && v_first != V_NONE && vfed - vq / 1000.0 > 0.2) st_play = 1;
        int stopped = was_play && !eof && picwatch_stopped(&pw, vq, vframes, started, t);
        /* (the same test as st_play, from this item's first frame; a new decoder stream queues more than it says
           before its first picture: also wait until it knows the picture size) */
        if (g_channel && !it_started && card_end < 0 && v_first != V_NONE && vfed - vq / 1000.0 - it_vfed0 > 0.2
            && (gap_t0 < 0 || stream_has_picture(vstr))) {
            it_started = 1; g_broke = 0; it_start_t = t;
            printf("item %d start\n", cur->seq);
            if (gap_t0 >= 0) { gap_t0 = -1; static_hide(); }
        }
        if (svol >= 0) {
            if (eof) static_hide();                   /* nothing more coming; its hiss would also keep aq > 0 */
            else if (card_end >= 0) ;                 /* the card is the static */
            else if (gap_t0 >= 0) {                   /* a new decoder stream fills before its first picture */
                if (!static_visible() && t - gap_t0 > 0.3) { static_show(vctx, 0); st_frames = vframes; }
            }
            else if (st_play && !was_play) static_hide();
            else if (static_visible()) { if (vframes >= st_frames + 3) static_hide(); }   /* frames flow again (vq may still read 0) */
            else if (stopped) { static_show(vctx, svol); st_frames = vframes; }
            static_tick(t);
        }
        /* where each output is now: audio ahead of the picture = positive */
        /* only once the picture is really moving: the decoder buffers ~1-2 s before it shows the first frame, and
           measuring during that gave several wrong corrections in a row */
        if (astr && v_first != V_NONE && a_end >= 0 && vq > 0 && aq > 0 && !adrop && vfed - vq / 1000.0 - it_vfed0 > 0.5 && !static_visible()
            && t >= sync_hold && !cur->over && drain_until < 0) {   /* (channel: not around a cut) */
            double v_play = v_first + vfed - vq / 1000.0;
            double a_play = a_end - afill / (double)(AUDIO_RATE * AUDIO_CH * 2) - aq / 1000.0;
            sync_sum += a_play - v_play + av_off; sync_n++;
        }
        /* the picture on screen now, as a time in this stream; zero = the requested start (see the top) */
        double shown = v_first != V_NONE ? v_first + vfed - vq / 1000.0 : -1;
        double zero = a_first >= 0 ? a_first : v_first;
        double start_s = cur->start_s;
        /* subtitle clock: the estimate above moves in steps (the decoder's queued-ms is coarse, up to ~1.5 s off), so
           run a steady clock on wall time, pulled gently towards the estimate and never backwards (cues would flicker
           at their edges); jump only when it is more than 1 s off (a stall) */
        if (subs && shown >= 0 && (a_first >= 0 || cur->ai < 0) && vfed - vq / 1000.0 > 0.5) {
            double est = start_s + shown - zero;
            double run = sub_clock + (t - sub_wall);
            if (sub_clock < 0 || run - est > 1.0 || est - run > 1.0) sub_clock = est;
            else { double c = run + 0.05 * (est - run); if (c > sub_clock) sub_clock = c; }
            sub_wall = t;
            subs_update(sub_clock);
        }
        read_ctl();
        osd_tick(shown >= 0 && (a_first >= 0 || cur->ai < 0) ? start_s + shown - zero : start_s);
        if (t - last >= 1.0) {
            int sw = 0, sh = 0; SLVideo_GetStreamResolution(vstr, &sw, &sh);
            if (sync_n) sync_last = sync_sum / sync_n;
            double pos = shown >= 0 ? shown - zero : 0;  /* seconds of the item played since the requested start */
            printf("t=%5.1f pos=%.1f video q=%4u ms frames=%ld fail=%ld %dx%d | audio q=%4u ms pkts=%ld | a-v %+5.0f ms fixes=%d | cpu %.0f%%",
                   t - start, pos < 0 ? 0 : pos, vq, vframes, vfail, sw, sh, aq, apk,
                   sync_last * 1000, sync_fixes, 100.0 * (cpu_s() - cpu0) / (t - last));
            pthread_mutex_lock(&q_mu);
            if (prebuf > 0) {
                double hv = -1, ha = -1;              /* stream time of the next video / audio packet to feed */
                for (int i = 0; i < cur->fifo_n && (hv < 0 || ha < 0); i++) {
                    int k = (cur->fifo_head + i) % FIFO_MAX;
                    if (cur->fifo[k]->stream_index == cur->vi && hv < 0) hv = cur->fifo_ts[k];
                    else if (cur->fifo[k]->stream_index == cur->ai && ha < 0) ha = cur->fifo_ts[k];
                }
                printf(" | ahead %.1f s (low %.1f) v %.1f a %.1f pk %d", cur->fifo_n ? cur->last_ts - cur->fifo_ts[cur->fifo_head] : 0,
                       fifo_low > 1e8 ? 0 : fifo_low, hv < 0 ? 0 : cur->last_ts - hv, ha < 0 ? 0 : cur->last_ts - ha, cur->fifo_n);
            }
            if (g_channel) {
                printf(" | item %d%s", cur->seq, card_end >= 0 ? " card" : "");
                printf(" mem %ldk", cur->bytes / 1024);   /* FIFO bytes of the item playing (+ the next, while open) */
                if (g_nx == NX_READY) printf("+%ldk", nxt->bytes / 1024);
            }
            printf(" | read max %.0f ms\n", read_max * 1000);
            fifo_low = 1e9; read_max = 0;
            pthread_mutex_unlock(&q_mu);
            fflush(stdout); last = t; cpu0 = cpu_s();
            if (sync_n >= 5 && t - sync_fix_t >= 2.0 && (sync_last * 1000 > SYNC_SLACK_MS || sync_last * 1000 < -SYNC_SLACK_MS)) {
                double ms = sync_last * 1000;
                if (ms > SYNC_MAX_MS) ms = SYNC_MAX_MS;
                if (ms < -SYNC_MAX_MS) ms = -SYNC_MAX_MS;
                int chunks = (int)((ms < 0 ? -ms : ms) / AUDIO_CHUNK_MS + 0.5);
                if (ms > 0) {                             /* sound early: hold it back with silence */
                    for (int i = 0; i < chunks; i++) { memset(SLAudio_BeginFrame(astr), 0, AUDIO_CHUNK_BYTES); SLAudio_SubmitFrame(astr); }
                } else adrop = (long)chunks * AUDIO_CHUNK_BYTES;   /* sound late: skip ahead */
                printf("sync: audio %s by %.0f ms, corrected\n", ms > 0 ? "early" : "late", ms < 0 ? -ms : ms);
                sync_fixes++; sync_fix_t = t;
            }
            sync_sum = 0; sync_n = 0;
        }
        if (g_channel && !eof) {
            /* --channel: when the item playing has nothing more to feed (read to its end and its FIFO empty, its
               limit reached, or the card's time up), the next takes over (cutplan.h) */
            chan_open(cur, nxt, card_end >= 0 ? (card_end > t ? card_end - t : 0) : -1);
            while (g_qn && g_q[g_qh].dead) queue_pop();  /* (reported when its open failed) */
            if (card_end >= 0 && t >= card_end) static_card(NULL);   /* its time is up (the next may still be opening) */
            /* the new item's sound starts in step with its picture: sound queued in SLAudio now plays when the picture
               timeline (the old item's frames still queued count before it) reaches `from`. Jellyfin's copied video
               starts at the keyframe before the requested start while the audio starts exactly there (a lead-in of
               up to several seconds: 6.8 s seen on an SD episode), and a new decoder stream queues ~1-3 s before its
               first picture (its queued-ms reads right only once it has shown pictures for a moment: 0.5 s). So the
               sound is held until the picture is within LEAD_GAP_S of its first sample, then
               starts after that much silence (or without what lies before `from`). */
            double from = v_first + vfed - vq / 1000.0 + aq / 1000.0;
            if (a_hold && a_first >= 0 && v_first != V_NONE && (a_hold == 1 || (it_started && t - it_start_t >= 0.5))
                && (a_first - from <= LEAD_GAP_S || g_hold_n >= HOLD_MAX - 1)) {
                double lead = a_first - from;
                if (lead > LEAD_GAP_S) lead = LEAD_GAP_S;
                int chunks = lead > 0 ? (int)(lead * 1000 / AUDIO_CHUNK_MS + 0.5) : 0;
                for (int i = 0; i < chunks; i++) { memset(SLAudio_BeginFrame(astr), 0, AUDIO_CHUNK_BYTES); SLAudio_SubmitFrame(astr); }
                int kept = hold_release(cur, from);
                printf("item %d: sound from %.2f s, %.0f ms silence first, %d packets held\n", cur->seq, from, chunks * (double)AUDIO_CHUNK_MS, kept);
                a_hold = 0; sync_hold = t + 3.0;
            }
            /* a decoder stream made again that shows no picture in GAP_GIVEUP_S: end the item (the static stays up) and
               make a fresh stream for the next one, rather than static until the channel is changed */
            if (gap_t0 >= 0 && !it_started && card_end < 0 && !cur->over && t - gap_t0 > GAP_GIVEUP_S) {
                printf("item %d: no picture after %.0f s, skipped\n", cur->seq, t - gap_t0);
                pthread_mutex_lock(&q_mu); cur->over = 1; pthread_mutex_unlock(&q_mu);
                force_reset = 1; g_broke = 1;
            }
            pthread_mutex_lock(&q_mu);
            int rerr = card_end < 0 ? cur->rd_err : 0;
            int out = card_end >= 0 ? t >= card_end : cur->over || (!cur->fifo_n && (cur->rd_eof || cur->rd_err));
            int read_out = card_end >= 0 || cur->over || cur->rd_eof || cur->rd_err;
            if (card_end < 0 && cur->rd_eof && !it_eof_said) { printf("end of stream\n"); it_eof_said = 1; }
            pthread_mutex_unlock(&q_mu);
            if (read_out && !g_qn && !q_empty_said) { printf("queue empty\n"); q_empty_said = 1; }
            if (out) {
                if (rerr && !it_err_said) {
                    printf("stream error: %s%s\n", av_err2str(rerr), g_qn ? ", cutting to the next item" : "");
                    it_err_said = 1; g_broke = 1;
                    if (!g_qn) { rc = 3; break; }
                }
                if (!g_qn) {                          /* sljfd is late: wait (the static comes up once the picture stops) */
                    if (g_ctl_ended) { eof = 1; if (g_broke) rc = 3; }   /* nothing more can come: play out and stop
                                                         (exit 3 when an error left nothing to play) */
                    else { usleep(20000); continue; }
                } else if (!g_q[g_qh].card && g_nx != NX_READY) { usleep(5000); continue; }   /* still opening */
                else {
                    struct pend *hd = &g_q[g_qh];
                    enum cut k = cut_kind(vw, vh, hd->card ? 0 : nxt->w, hd->card ? 0 : nxt->h, hd->card);
                    if (card_end >= 0 && k == CUT_SAME) k = CUT_RESET;   /* after a card: a fresh decoder stream (see below) */
                    if (force_reset && k == CUT_SAME) k = CUT_RESET;     /* the stream showed nothing (see GAP_GIVEUP_S) */
                    force_reset = 0;
                    if (k != CUT_SAME && card_end < 0) {   /* the old picture plays out first (vq does not always reach 0) */
                        if (drain_until < 0) drain_until = t + vq / 1000.0 + 1.0;
                        if (vq > 0 && t < drain_until) { usleep(5000); continue; }
                    }
                    drain_until = -1;
                    hold_clear(); a_hold = 0;
                    printf("item %d end\n", cur->seq);
                    if (card_end >= 0) { static_card(NULL); card_end = -1; }
                    else {
                        double c0 = now_s();
                        item_close(cur);
                        if (now_s() - c0 > 0.05) printf("item %d closed in %.0f ms\n", cur->seq, (now_s() - c0) * 1000);
                    }
                    if (k == CUT_CARD) {              /* silent static with the text; the decoder stream stays */
                        memset(cur, 0, sizeof *cur);
                        cur->seq = hd->seq; cur->kind = 1; cur->vi = cur->ai = -1;
                        card_end = t + hd->limit;
                        static_show(vctx, 0); st_frames = vframes;
                        static_card(hd->arg);
                        printf("item %d start\n", cur->seq);
                        queue_pop();
                        continue;
                    }
                    struct item *o = cur; cur = nxt; nxt = o;   /* a video takes over */
                    g_nx = NX_NONE; queue_pop();
                    pthread_mutex_lock(&q_mu); cur->ahead = g_ahead; pthread_cond_broadcast(&q_cv); pthread_mutex_unlock(&q_mu);
                    vs = cur->fmt->streams[cur->vi];
                    v_acc += (vframes - n_base) * fdur; n_base = vframes;   /* (video fed so far stays counted) */
                    fr = item_rate(cur);
                    if (fr.num) fdur = av_q2d(av_inv_q(fr));
                    if (k == CUT_RESET) {
                        /* another size (or after a card, when the stream has sat empty: a starved SLVideo stream races
                           through what it is given next): free the decoder stream and make one for this item */
                        SLVideo_FreeStream(vstr);
                        vstr = video_stream_new(vctx, cur);
                        if (!vstr) { rc = 1; break; }
                        printf("video stream made again for %dx%d\n", cur->w, cur->h);
                        gap_t0 = t;
                    } else if (fr.num) SLVideo_SetStreamTargetFramerate(vstr, fr.num, fr.den);
                    vw = cur->w; vh = cur->h;
                    /* the clock starts over from this item's timestamps with the running frame count */
                    v_first = V_NONE; a_first = a_end = -1; dts_n = 0;
                    afill = 0; adrop = 0;             /* (under 20 ms of the old item's sound left in abuf) */
                    sync_sum = 0; sync_n = 0; sync_hold = t + 3.0;
                    it_vfed0 = v_acc; it_started = it_eof_said = it_err_said = q_empty_said = 0;
                    if (cur->adec && !astr) {         /* the first item had no sound */
                        if (audio_stream_open(&actx, &astr) < 0) { rc = 1; break; }
                    }
                    if (cur->adec) a_hold = k == CUT_SAME ? 1 : 2;
                    printf("cut to item %d: %s, %dx%d\n", cur->seq, k == CUT_SAME ? "same stream" : "new stream", cur->w, cur->h);
                    continue;
                }
            } else if (card_end >= 0) { usleep(5000); continue; }   /* a card feeds nothing */
        }
        pthread_mutex_lock(&q_mu);
        if (cur->threaded && !g_channel) {                /* the reader thread's end / error, once the FIFO is empty */
            if (cur->rd_err && !cur->fifo_n) { err = cur->rd_err; pthread_mutex_unlock(&q_mu); printf("stream error: %s\n", av_err2str(err)); rc = 3; break; }
            if (cur->rd_eof && !eof) { printf("end of stream\n"); eof = 1; }
        }
        int n_now = cur->over ? 0 : cur->fifo_n;          /* (channel: past its limit the rest is not fed) */
        pthread_mutex_unlock(&q_mu);
        /* end of stream: wait for the decoders to play out what they hold. The video queue does not always reach 0
           (a re-encoded episode sat at its last frame for 20 min, 2026-11-20), so also stop once it has had the time
           it said it needed plus 3 s */
        if (eof && !n_now) {
            if (eof_wait < 0) eof_wait = t + (vq > aq ? vq : aq) / 1000.0 + 3.0;
            if ((vq == 0 && aq == 0) || t > eof_wait) { printf("played out%s\n", vq || aq ? " (decoder queue stuck, stopped waiting)" : ""); break; }
            usleep(20000); continue;
        }
        if (cur->over) { usleep(5000); continue; }        /* channel: limit reached, waiting for the next */
        /* back-pressure: both decoders hold about AHEAD_MS; no room while both are full */
        /* a decoder stream made again shows nothing until it holds enough: it may need more than AHEAD_MS (a 29.97 fps
           stick clip after a 1080p episode sat at 1063 ms queued with no picture, and nothing more was fed: stuck on
           static, 2026-11-21), so until its first picture it may take up to GAP_AHEAD_MS */
        uint32_t ahead_ms = gap_t0 >= 0 && !it_started ? GAP_AHEAD_MS : AHEAD_MS;
        int room = !(vq > ahead_ms && (!astr || aq > AHEAD_MS || a_hold));   /* (sound held after a cut: video alone) */
        /* inline read (no prebuffer): one packet when the decoders have room */
        int want_read = !cur->threaded && !eof && cur->fifo_n < FIFO_MAX && (prebuf > 0 ? fifo_span(cur) < g_ahead : (!cur->fifo_n && room));
        if (want_read) {
            double r0 = now_s();
            err = av_read_frame(cur->fmt, pkt);
            if (now_s() - r0 > read_max) read_max = now_s() - r0;
            if (err == AVERROR_EOF) { printf("end of stream\n"); eof = 1; continue; }
            if (err < 0) { printf("stream error: %s\n", av_err2str(err)); rc = 3; break; }
            fifo_push(cur, pkt);
        }
        pthread_mutex_lock(&q_mu);
        double span = fifo_span(cur);
        if (!started) {
            if (span >= prebuf || eof || cur->fifo_n == FIFO_MAX) {
                started = 1;
                if (prebuf > 0) printf("prebuffered %.1f s in %.1f s\n", span, now_s() - start);
            } else { pthread_mutex_unlock(&q_mu); if (cur->threaded) usleep(5000); continue; }
        }
        if (prebuf > 0 && cur->fifo_n) {
            if (span < fifo_low) fifo_low = span;
            if (span < 0.2 && !low_logged) { printf("read-ahead ran out (network slower than real time)\n"); low_logged = 1; }
            if (span > 1.0) low_logged = 0;
        }
        if (!room || !cur->fifo_n) { pthread_mutex_unlock(&q_mu); if (!want_read) usleep(5000); continue; }
        if (cur->limit_s > 0 && cur->fifo_ts[cur->fifo_head] - cur->first_ts >= cur->limit_s) {
            cur->over = 1;                                /* channel: the item's limit (stream time); feed no more of it */
            pthread_mutex_unlock(&q_mu);
            printf("item %d: limit %.0f s reached\n", cur->seq, cur->limit_s);
            continue;
        }
        cur->bytes -= cur->fifo[cur->fifo_head]->size;
        av_packet_move_ref(pkt, cur->fifo[cur->fifo_head]);
        av_packet_free(&cur->fifo[cur->fifo_head]);
        cur->fifo_head = (cur->fifo_head + 1) % FIFO_MAX; cur->fifo_n--;
        pthread_cond_broadcast(&q_cv);                    /* (two readers may wait: wake both, each checks its own FIFO) */
        pthread_mutex_unlock(&q_mu);

        if (pkt->stream_index == cur->vi) {
            AVPacket *p = pkt;
            if (v_first == V_NONE && pkt->pts != AV_NOPTS_VALUE) v_first = pkt->pts * av_q2d(vs->time_base) - vfed;
            if (pkt->dts != AV_NOPTS_VALUE) {                /* measure the real frame rate (see fdur above) */
                double d = pkt->dts * av_q2d(vs->time_base);
                /* timestamps went back (new segment) or jumped ahead (the server skipped part of the stream, seen
                   2026-11-21 as "23.976 -> 9.163 fps" = slow motion): start over, before the jump is averaged in */
                if (dts_n > 0 && (d < dts_ring[dts_n - 1] || d - dts_ring[dts_n - 1] > 0.5)) dts_n = 0;
                if (dts_n == 60) {
                    double avg = (d - dts_ring[0]) / 60.0;
                    if (avg > 1.0 / 121 && avg < 0.2 && (avg - fdur > 0.03 * fdur || fdur - avg > 0.03 * fdur)) {
                        v_acc += (vframes - n_base) * fdur; n_base = vframes;
                        printf("frame rate %.3f -> %.3f fps\n", 1 / fdur, 1 / avg);
                        fdur = avg;
                        SLVideo_SetStreamTargetFramerate(vstr, (int)(1000.0 / avg + 0.5), 1000);
                        dts_n = 0;                            /* judge the new rate on 60 fresh frames, not again now */
                    } else { memmove(dts_ring, dts_ring + 1, 59 * sizeof(double)); dts_n = 59; }
                }
                dts_ring[dts_n++] = d;
            }
            if (cur->bsf) {
                av_bsf_send_packet(cur->bsf, pkt);
                while (av_bsf_receive_packet(cur->bsf, fpkt) == 0) {
                    if (SLVideo_BeginFrame(vstr, fpkt->size) || SLVideo_WriteFrameData(vstr, fpkt->data, fpkt->size)
                        || SLVideo_SubmitFrame(vstr)) vfail++; else vframes++;
                    av_packet_unref(fpkt);
                }
                p = NULL;
            }
            if (p) {
                if (SLVideo_BeginFrame(vstr, p->size) || SLVideo_WriteFrameData(vstr, p->data, p->size)
                    || SLVideo_SubmitFrame(vstr)) vfail++; else vframes++;
            }
        } else if (pkt->stream_index == cur->ai && cur->adec && a_hold) {   /* channel: held after a cut (see above) */
            if (a_first < 0 && pkt->pts != AV_NOPTS_VALUE) a_first = pkt->pts * av_q2d(cur->fmt->streams[cur->ai]->time_base);
            hold_push(pkt);
        } else if (pkt->stream_index == cur->ai && cur->adec) {
            apk++;
            if (a_first < 0 && pkt->pts != AV_NOPTS_VALUE) a_first = pkt->pts * av_q2d(cur->fmt->streams[cur->ai]->time_base);
            if (avcodec_send_packet(cur->adec, pkt) == 0) {
                while (avcodec_receive_frame(cur->adec, afr) == 0) {
                    if (!cur->swr) {                       /* set up from the first decoded frame */
                        AVChannelLayout out = AV_CHANNEL_LAYOUT_STEREO;
                        swr_alloc_set_opts2(&cur->swr, &out, AV_SAMPLE_FMT_S16, AUDIO_RATE,
                                            &afr->ch_layout, afr->format, afr->sample_rate, 0, NULL);
                        if (!cur->swr || swr_init(cur->swr) < 0) { fprintf(stderr, "resampler init failed\n"); return 1; }
                        printf("audio started: %d ch %d Hz\n", afr->ch_layout.nb_channels, afr->sample_rate);
                    }
                    int room = (int)(sizeof abuf - afill) / (AUDIO_CH * 2);
                    uint8_t *outp = abuf + afill;
                    int n = swr_convert(cur->swr, &outp, room, (const uint8_t **)afr->extended_data, afr->nb_samples);
                    if (n > 0) afill += n * AUDIO_CH * 2;
                    if (afr->pts != AV_NOPTS_VALUE)
                        a_end = afr->pts * av_q2d(cur->fmt->streams[cur->ai]->time_base) + afr->nb_samples / (double)afr->sample_rate;
                    if (adrop > 0) {
                        int k = adrop < afill ? (int)adrop : afill;
                        afill -= k; adrop -= k; memmove(abuf, abuf + k, afill);
                    }
                    while (afill >= AUDIO_CHUNK_BYTES) {
                        uint8_t *fr = SLAudio_BeginFrame(astr);
                        memcpy(fr, abuf, AUDIO_CHUNK_BYTES);
                        apply_gain(fr, AUDIO_CHUNK_BYTES);
                        SLAudio_SubmitFrame(astr);
                        static_prog_audio();          /* programme sound: the static's hiss stops */
                        afill -= AUDIO_CHUNK_BYTES;
                        memmove(abuf, abuf + AUDIO_CHUNK_BYTES, afill);
                    }
                    av_frame_unref(afr);
                }
            }
        }
        av_packet_unref(pkt);
    }

    if (g_channel && eof && (!rc || g_broke) && !g_stop) printf("item %d end\n", cur->seq);
    hold_clear();
    if (g_nx == NX_OPENING) pthread_join(g_op_th, NULL);   /* (g_stop ends a blocked open through the interrupt callback) */
    if (g_nx != NX_NONE) item_close(nxt);
    item_close(cur);                                  /* the reader thread (the interrupt callback ends a read in progress), the FIFO, the demuxer, filter and decoder */
    av_dict_free(&g_opts);
    printf("stopped after %.1f s: %ld video frames (%ld failed), %ld audio packets\n",
           now_s() - start, vframes, vfail, apk);
    static_free();                                    /* before the contexts go; the shared stream is ours now */
    subs_free();                                      /* subtitles and the bar */
    if (astr) SLAudio_FreeStream(astr);
    if (actx) SLAudio_FreeContext(actx);
    if (vstr) SLVideo_FreeStream(vstr);
    SLVideo_FreeContext(vctx);
    av_frame_free(&afr); av_packet_free(&pkt); av_packet_free(&fpkt);
    return rc;
}
