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
 *   stdin: control lines from sljfd, "osd ..." = the on-screen bar (sljf-subs.c, osd_command). End of stdin is
 *   ignored when playing (sljf-test gives none) and ends --card.
 *   SIGTERM stops it.  Exit code: 0 ended/stopped, 3 the stream broke.
 *   No pause: starving SLVideo does not pause it (its queue clock keeps running, tried 2026-11-19, the sync then
 *   threw away audio non-stop). sljfd pauses by stopping sljf-play and starting it again at the same position.
 *   env SLJF_HEADERS   extra HTTP headers ("Authorization: ...\r\n"), so the token is not in the URL
 *   env SLJF_UA        User-Agent (Cloudflare in front of the server blocks some defaults)
 *   env SLJF_CA        CA bundle for https (the Link's own list is from 2013); verification is on when set
 *   env SLJF_AV_OFFSET_MS  fine-tune lip sync: positive plays the sound later, negative earlier (default 0)
 *   env SLJF_START     where in the item the stream starts (seconds; Jellyfin's StartTimeTicks), for subtitle timing
 *   env SLJF_PREBUFFER_MS  hold this much stream before feeding the decoders and keep reading that far ahead (Live
 *                      TV: it arrives at exactly real time, so without a cushion every network hiccup empties the
 *                      audio queue, the sound falls behind and the sync cuts it = stutter). 0 = off (files, default)
 *   env SLJF_SUBS      SRT file: draw these subtitles on an SLVideo overlay (sljf-subs.c); SLJF_SUB_FONT the TTF
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

#define AUDIO_RATE 48000
#define AUDIO_CH 2
#define AUDIO_CHUNK_MS 20
#define AUDIO_CHUNK_SAMPLES (AUDIO_RATE * AUDIO_CHUNK_MS / 1000)
#define AUDIO_CHUNK_BYTES (AUDIO_CHUNK_SAMPLES * AUDIO_CH * 2)
#define AHEAD_MS 1000            /* keep about this much queued in each decoder */
#define SYNC_SLACK_MS 45         /* leave smaller A/V offsets alone */
#define SYNC_MAX_MS 3000         /* largest single correction */

#define FIFO_MAX 4096            /* read-ahead packets (~25 s of 25 fps video + audio) */

int subs_init(CSLVideoContext *vctx);    /* sljf-subs.c */
void subs_update(double t);
void subs_free(void);
void osd_command(const char *line);
void osd_tick(double t);

static volatile int g_stop;
static void on_signal(int s) { (void)s; g_stop = 1; }

/* Read-ahead FIFO (SLJF_PREBUFFER_MS, Live TV): packets read but not yet fed to the decoders, with their stream time.
   With a prebuffer a reader thread fills it, so a slow read never holds up feeding: Live TV at the live edge waits in
   av_read_frame for the network, and at times the server sends in bursts (Cartoon Network 2026-11-20: every read
   waited 230-490 ms, twice 2.9 s, for 3 min); when that wait was in the feeding loop both decoders ran dry with 3.8 s
   of packets in this FIFO (stutter, 60 sync corrections). Without a prebuffer (files) the loop reads inline. */
static AVPacket *fifo[FIFO_MAX]; static double fifo_ts[FIFO_MAX];
static int fifo_head, fifo_n;
static double last_ts, read_max;                      /* newest packet's stream time; longest read since the status */
static AVFormatContext *g_fmt; static double g_prebuf;
static int rd_eof, rd_err;                            /* the reader thread saw the end / a stream error */
static volatile sig_atomic_t rd_quit;
static pthread_mutex_t q_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t q_cv = PTHREAD_COND_INITIALIZER;

static double now_s(void);
static int interrupt_cb(void *o) { (void)o; return g_stop || rd_quit; }   /* lets a blocked read end on stop */

static double fifo_span(void) { return fifo_n ? last_ts - fifo_ts[fifo_head] : 0; }

/* one packet into the FIFO (q_mu held) */
static void fifo_push(AVPacket *pkt) {
    int64_t ts = pkt->pts != AV_NOPTS_VALUE ? pkt->pts : pkt->dts;
    if (ts != AV_NOPTS_VALUE) last_ts = ts * av_q2d(g_fmt->streams[pkt->stream_index]->time_base);
    AVPacket *q = av_packet_alloc();
    av_packet_move_ref(q, pkt);
    int at = (fifo_head + fifo_n) % FIFO_MAX;
    fifo[at] = q; fifo_ts[at] = last_ts; fifo_n++;
}

static void *reader(void *o) {
    (void)o;
    AVPacket *pkt = av_packet_alloc();
    for (;;) {
        pthread_mutex_lock(&q_mu);
        while (!rd_quit && !g_stop && (fifo_n >= FIFO_MAX || fifo_span() >= g_prebuf + 1.0)) {
            struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_nsec += 100000000; if (ts.tv_nsec >= 1000000000) { ts.tv_sec++; ts.tv_nsec -= 1000000000; }
            pthread_cond_timedwait(&q_cv, &q_mu, &ts);
        }
        int quit = rd_quit || g_stop;
        pthread_mutex_unlock(&q_mu);
        if (quit) break;
        double r0 = now_s();
        int err = av_read_frame(g_fmt, pkt);
        double w = now_s() - r0;
        pthread_mutex_lock(&q_mu);
        if (w > read_max) read_max = w;
        if (err < 0) {
            if (err == AVERROR_EOF) rd_eof = 1; else rd_err = err;
            pthread_mutex_unlock(&q_mu);
            break;
        }
        fifo_push(pkt);
        pthread_mutex_unlock(&q_mu);
    }
    av_packet_free(&pkt);
    return NULL;
}

static double now_s(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9;
}
static double cpu_s(void) {
    struct timespec t; clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t); return t.tv_sec + t.tv_nsec / 1e9;
}

/* read whatever sljfd has written to stdin and act on complete lines; returns -1 once stdin has ended */
static int read_ctl(void) {
    static char buf[4096]; static int fill, ended, set;
    if (ended) return -1;
    if (!set) { fcntl(0, F_SETFL, fcntl(0, F_GETFL) | O_NONBLOCK); set = 1; }
    for (;;) {
        ssize_t n = read(0, buf + fill, sizeof buf - 1 - fill);
        if (n == 0) { ended = 1; return -1; }
        if (n < 0) return errno == EAGAIN || errno == EWOULDBLOCK ? 0 : (ended = 1, -1);
        fill += (int)n; buf[fill] = 0;
        char *nl;
        while ((nl = strchr(buf, '\n'))) {
            *nl = 0;
            osd_command(buf);
            fill -= (int)(nl + 1 - buf);
            memmove(buf, nl + 1, fill + 1);
        }
        if (fill >= (int)sizeof buf - 1) fill = 0;     /* a line too long: drop it */
    }
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

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "--version")) { puts(SLJF_VERSION); return 0; }
    if (argc < 2) { fprintf(stderr, "usage: sljf-play <url> [seconds] | --card | --version\n"); return 2; }
    if (!strcmp(argv[1], "--card")) {
        signal(SIGINT, on_signal); signal(SIGTERM, on_signal); setvbuf(stdout, NULL, _IOLBF, 0);
        return card();
    }
    const char *url = argv[1];
    double limit = argc > 2 ? atof(argv[2]) : 0;
    signal(SIGINT, on_signal); signal(SIGTERM, on_signal);
    setvbuf(stdout, NULL, _IOLBF, 0);                   /* status lines show up over ssh without a tty */
    if (getenv("SLJF_DEBUG")) av_log_set_level(AV_LOG_DEBUG);

    AVDictionary *opts = NULL;
    const char *h = getenv("SLJF_HEADERS"), *ua = getenv("SLJF_UA");
    if (h) av_dict_set(&opts, "headers", h, 0);
    av_dict_set(&opts, "user_agent", ua ? ua : "SteamLinkJellyfin/" SLJF_VERSION, 0);
    const char *ca = getenv("SLJF_CA");                /* current CA list shipped with the app */
    if (ca) { av_dict_set(&opts, "ca_file", ca, 0); av_dict_set(&opts, "tls_verify", "1", 0); }
    av_dict_set(&opts, "rw_timeout", "15000000", 0);  /* 15 s: give up instead of hanging */
    /* no "reconnect": Jellyfin's stream cannot be resumed by byte offset. On a dropped connection sljf-play exits
       with code 3 and sljfd starts it again at the last position. */

    AVFormatContext *fmt = avformat_alloc_context();
    fmt->interrupt_callback.callback = interrupt_cb;   /* copied into the connection at open: set it first */
    double t0 = now_s();
    int err = avformat_open_input(&fmt, url, NULL, &opts);
    av_dict_free(&opts);
    if (err < 0) { fprintf(stderr, "open failed: %s\n", av_err2str(err)); return 1; }
    if ((err = avformat_find_stream_info(fmt, NULL)) < 0) { fprintf(stderr, "no stream info: %s\n", av_err2str(err)); return 1; }
    printf("opened in %.1f s, format %s\n", now_s() - t0, fmt->iformat->name);

    int vi = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    int ai = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, NULL, 0);
    if (vi < 0) { fprintf(stderr, "no video stream\n"); return 1; }
    /* Jellyfin's audio can start late in the stream (it converts audio while copying video), so probing may see an
       audio stream without parameters and av_find_best_stream skips it: take it anyway, the decoder works them out. */
    for (unsigned i = 0; ai < 0 && i < fmt->nb_streams; i++)
        if (fmt->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_AUDIO) ai = (int)i;
    AVStream *vs = fmt->streams[vi];
    if (vs->codecpar->codec_id != AV_CODEC_ID_H264) {
        fprintf(stderr, "video is %s, the Steam Link decoder only takes h264\n", avcodec_get_name(vs->codecpar->codec_id));
        return 1;
    }
    printf("video h264 %dx%d profile %d, %.3f fps\n", vs->codecpar->width, vs->codecpar->height,
           vs->codecpar->profile, av_q2d(vs->avg_frame_rate));

    /* MP4/MKV carry length-prefixed NALs: convert to Annex-B (MPEG-TS already is). */
    AVBSFContext *bsf = NULL;
    if (vs->codecpar->extradata_size && vs->codecpar->extradata[0] == 1) {
        const AVBitStreamFilter *f = av_bsf_get_by_name("h264_mp4toannexb");
        av_bsf_alloc(f, &bsf);
        avcodec_parameters_copy(bsf->par_in, vs->codecpar);
        bsf->time_base_in = vs->time_base;
        av_bsf_init(bsf);
        printf("converting to Annex-B\n");
    }

    CSLVideoContext *vctx = SLVideo_CreateContext();
    if (!vctx) { fprintf(stderr, "SLVideo_CreateContext failed\n"); return 1; }
    int dw = 0, dh = 0; SLVideo_GetDisplayResolution(vctx, &dw, &dh);
    printf("display %dx%d\n", dw, dh);
    CSLVideoStream *vstr = SLVideo_CreateStream(vctx, k_ESLVideoFormatH264, 0);   /* 0: B-frames allowed, queued */
    if (!vstr) { fprintf(stderr, "SLVideo_CreateStream failed\n"); return 1; }
    SLVideo_SetStreamVideoTransferMatrix(vstr, k_ESLVideoTransferMatrix_Automatic);
    AVRational fr = vs->avg_frame_rate.num ? vs->avg_frame_rate : vs->r_frame_rate;
    if (fr.num) SLVideo_SetStreamTargetFramerate(vstr, fr.num, fr.den);
    if (fmt->bit_rate > 0) SLVideo_SetStreamTargetBitrate(vstr, (int)(fmt->bit_rate / 1000));
    int subs = subs_init(vctx) == 0;
    double start_s = getenv("SLJF_START") ? atof(getenv("SLJF_START")) : 0;

    AVCodecContext *adec = NULL; SwrContext *swr = NULL;
    CSLAudioContext *actx = NULL; CSLAudioStream *astr = NULL;
    if (ai >= 0) {
        AVStream *as = fmt->streams[ai];
        const AVCodec *c = avcodec_find_decoder(as->codecpar->codec_id);
        if (!c) { fprintf(stderr, "no decoder for audio %s, playing without sound\n", avcodec_get_name(as->codecpar->codec_id)); ai = -1; }
        else {
            adec = avcodec_alloc_context3(c);
            avcodec_parameters_to_context(adec, as->codecpar);
            if (avcodec_open2(adec, c, NULL) < 0) { fprintf(stderr, "audio decoder open failed\n"); return 1; }
            actx = SLAudio_CreateContext();
            astr = actx ? SLAudio_CreateStream(actx, AUDIO_RATE, AUDIO_CH, AUDIO_CHUNK_BYTES, 0) : NULL;
            if (!astr) { fprintf(stderr, "SLAudio stream failed\n"); return 1; }
            printf("audio %s -> s16 stereo 48 kHz\n", c->name);
        }
    }

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
    double v_first = -1, a_end = -1;                 /* pts (s) of the first video frame / end of decoded audio */
    double a_first = -1;                              /* pts (s) of the first audio packet = the requested start */
    double sub_clock = -1, sub_wall = 0;              /* subtitle clock (item seconds) and when it was last set */
    int started = 0, low_logged = 0;
    double prebuf = getenv("SLJF_PREBUFFER_MS") ? atof(getenv("SLJF_PREBUFFER_MS")) / 1000.0 : 0;
    double fifo_low = 1e9;                            /* smallest read-ahead since the last status line */
    g_fmt = fmt; g_prebuf = prebuf;
    pthread_t rd_th; int threaded = 0;
    if (prebuf > 0) {
        if (pthread_create(&rd_th, NULL, reader, NULL) == 0) threaded = 1;
        else printf("reader thread failed, reading inline\n");
    }
    double sync_sum = 0, sync_last = 0, sync_fix_t = 0; int sync_n = 0, sync_fixes = 0;
    long adrop = 0;                                   /* bytes of PCM still to throw away (audio was late) */
    double av_off = getenv("SLJF_AV_OFFSET_MS") ? atof(getenv("SLJF_AV_OFFSET_MS")) / 1000.0 : 0;
    double start = now_s(), last = start, cpu0 = cpu_s();
    int eof = 0, rc = 0;
    double eof_wait = -1;                             /* when to stop waiting for the decoders after the end */

    while (!g_stop) {
        double t = now_s();
        if (limit > 0 && t - start > limit) break;
        double vfed = v_acc + (vframes - n_base) * fdur;     /* seconds of video handed to the decoder */
        uint32_t vq = SLVideo_GetQueuedVideoMS(vstr);
        uint32_t aq = astr ? SLAudio_GetQueuedAudioSamples(astr) * 1000 / AUDIO_RATE : 0;
        /* where each output is now: audio ahead of the picture = positive */
        /* only once the picture is really moving: the decoder buffers ~1-2 s before it shows the first frame, and
           measuring during that gave several wrong corrections in a row */
        if (astr && v_first >= 0 && a_end >= 0 && vq > 0 && aq > 0 && !adrop && vfed - vq / 1000.0 > 0.5) {
            double v_play = v_first + vfed - vq / 1000.0;
            double a_play = a_end - afill / (double)(AUDIO_RATE * AUDIO_CH * 2) - aq / 1000.0;
            sync_sum += a_play - v_play + av_off; sync_n++;
        }
        /* the picture on screen now, as a time in this stream; zero = the requested start (see the top) */
        double shown = v_first >= 0 ? v_first + vfed - vq / 1000.0 : -1;
        double zero = a_first >= 0 ? a_first : v_first;
        /* subtitle clock: the estimate above moves in steps (the decoder's queued-ms is coarse, up to ~1.5 s off), so
           run a steady clock on wall time, pulled gently towards the estimate and never backwards (cues would flicker
           at their edges); jump only when it is more than 1 s off (a stall) */
        if (subs && shown >= 0 && (a_first >= 0 || ai < 0) && vfed - vq / 1000.0 > 0.5) {
            double est = start_s + shown - zero;
            double run = sub_clock + (t - sub_wall);
            if (sub_clock < 0 || run - est > 1.0 || est - run > 1.0) sub_clock = est;
            else { double c = run + 0.05 * (est - run); if (c > sub_clock) sub_clock = c; }
            sub_wall = t;
            subs_update(sub_clock);
        }
        read_ctl();
        osd_tick(shown >= 0 && (a_first >= 0 || ai < 0) ? start_s + shown - zero : start_s);
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
                for (int i = 0; i < fifo_n && (hv < 0 || ha < 0); i++) {
                    int k = (fifo_head + i) % FIFO_MAX;
                    if (fifo[k]->stream_index == vi && hv < 0) hv = fifo_ts[k];
                    else if (fifo[k]->stream_index == ai && ha < 0) ha = fifo_ts[k];
                }
                printf(" | ahead %.1f s (low %.1f) v %.1f a %.1f pk %d", fifo_n ? last_ts - fifo_ts[fifo_head] : 0,
                       fifo_low > 1e8 ? 0 : fifo_low, hv < 0 ? 0 : last_ts - hv, ha < 0 ? 0 : last_ts - ha, fifo_n);
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
        pthread_mutex_lock(&q_mu);
        if (threaded) {                               /* the reader thread's end / error, once the FIFO is empty */
            if (rd_err && !fifo_n) { err = rd_err; pthread_mutex_unlock(&q_mu); printf("stream error: %s\n", av_err2str(err)); rc = 3; break; }
            if (rd_eof && !eof) { printf("end of stream\n"); eof = 1; }
        }
        int n_now = fifo_n;
        pthread_mutex_unlock(&q_mu);
        /* end of stream: wait for the decoders to play out what they hold. The video queue does not always reach 0
           (a re-encoded episode sat at its last frame for 20 min, 2026-11-20), so also stop once it has had the time
           it said it needed plus 3 s */
        if (eof && !n_now) {
            if (eof_wait < 0) eof_wait = t + (vq > aq ? vq : aq) / 1000.0 + 3.0;
            if ((vq == 0 && aq == 0) || t > eof_wait) { printf("played out%s\n", vq || aq ? " (decoder queue stuck, stopped waiting)" : ""); break; }
            usleep(20000); continue;
        }
        /* back-pressure: both decoders hold about AHEAD_MS; no room while both are full */
        int room = !(vq > AHEAD_MS && (!astr || aq > AHEAD_MS));
        /* inline read (no prebuffer): one packet when the decoders have room */
        int want_read = !threaded && !eof && fifo_n < FIFO_MAX && (prebuf > 0 ? fifo_span() < prebuf + 1.0 : (!fifo_n && room));
        if (want_read) {
            double r0 = now_s();
            err = av_read_frame(fmt, pkt);
            if (now_s() - r0 > read_max) read_max = now_s() - r0;
            if (err == AVERROR_EOF) { printf("end of stream\n"); eof = 1; continue; }
            if (err < 0) { printf("stream error: %s\n", av_err2str(err)); rc = 3; break; }
            fifo_push(pkt);
        }
        pthread_mutex_lock(&q_mu);
        double span = fifo_span();
        if (!started) {
            if (span >= prebuf || eof || fifo_n == FIFO_MAX) {
                started = 1;
                if (prebuf > 0) printf("prebuffered %.1f s in %.1f s\n", span, now_s() - start);
            } else { pthread_mutex_unlock(&q_mu); if (threaded) usleep(5000); continue; }
        }
        if (prebuf > 0 && fifo_n) {
            if (span < fifo_low) fifo_low = span;
            if (span < 0.2 && !low_logged) { printf("read-ahead ran out (network slower than real time)\n"); low_logged = 1; }
            if (span > 1.0) low_logged = 0;
        }
        if (!room || !fifo_n) { pthread_mutex_unlock(&q_mu); if (!want_read) usleep(5000); continue; }
        av_packet_move_ref(pkt, fifo[fifo_head]);
        av_packet_free(&fifo[fifo_head]);
        fifo_head = (fifo_head + 1) % FIFO_MAX; fifo_n--;
        pthread_cond_signal(&q_cv);
        pthread_mutex_unlock(&q_mu);

        if (pkt->stream_index == vi) {
            AVPacket *p = pkt;
            if (v_first < 0 && pkt->pts != AV_NOPTS_VALUE) v_first = pkt->pts * av_q2d(vs->time_base) - vframes * fdur;
            if (pkt->dts != AV_NOPTS_VALUE) {                /* measure the real frame rate (see fdur above) */
                double d = pkt->dts * av_q2d(vs->time_base);
                if (dts_n == 60) {
                    double avg = (d - dts_ring[0]) / 60.0;
                    if (avg > 1.0 / 121 && avg < 0.2 && (avg - fdur > 0.03 * fdur || fdur - avg > 0.03 * fdur)) {
                        v_acc += (vframes - n_base) * fdur; n_base = vframes;
                        printf("frame rate %.3f -> %.3f fps\n", 1 / fdur, 1 / avg);
                        fdur = avg;
                        SLVideo_SetStreamTargetFramerate(vstr, (int)(1000.0 / avg + 0.5), 1000);
                    }
                    memmove(dts_ring, dts_ring + 1, 59 * sizeof(double)); dts_n = 59;
                }
                if (dts_n > 0 && d < dts_ring[dts_n - 1]) dts_n = 0;   /* timestamps went back (new segment): start over */
                dts_ring[dts_n++] = d;
            }
            if (bsf) {
                av_bsf_send_packet(bsf, pkt);
                while (av_bsf_receive_packet(bsf, fpkt) == 0) {
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
        } else if (pkt->stream_index == ai && adec) {
            apk++;
            if (a_first < 0 && pkt->pts != AV_NOPTS_VALUE) a_first = pkt->pts * av_q2d(fmt->streams[ai]->time_base);
            if (avcodec_send_packet(adec, pkt) == 0) {
                while (avcodec_receive_frame(adec, afr) == 0) {
                    if (!swr) {                       /* set up from the first decoded frame */
                        AVChannelLayout out = AV_CHANNEL_LAYOUT_STEREO;
                        swr_alloc_set_opts2(&swr, &out, AV_SAMPLE_FMT_S16, AUDIO_RATE,
                                            &afr->ch_layout, afr->format, afr->sample_rate, 0, NULL);
                        if (!swr || swr_init(swr) < 0) { fprintf(stderr, "resampler init failed\n"); return 1; }
                        printf("audio started: %d ch %d Hz\n", afr->ch_layout.nb_channels, afr->sample_rate);
                    }
                    int room = (int)(sizeof abuf - afill) / (AUDIO_CH * 2);
                    uint8_t *outp = abuf + afill;
                    int n = swr_convert(swr, &outp, room, (const uint8_t **)afr->extended_data, afr->nb_samples);
                    if (n > 0) afill += n * AUDIO_CH * 2;
                    if (afr->pts != AV_NOPTS_VALUE)
                        a_end = afr->pts * av_q2d(fmt->streams[ai]->time_base) + afr->nb_samples / (double)afr->sample_rate;
                    if (adrop > 0) {
                        int k = adrop < afill ? (int)adrop : afill;
                        afill -= k; adrop -= k; memmove(abuf, abuf + k, afill);
                    }
                    while (afill >= AUDIO_CHUNK_BYTES) {
                        memcpy(SLAudio_BeginFrame(astr), abuf, AUDIO_CHUNK_BYTES);
                        SLAudio_SubmitFrame(astr);
                        afill -= AUDIO_CHUNK_BYTES;
                        memmove(abuf, abuf + AUDIO_CHUNK_BYTES, afill);
                    }
                    av_frame_unref(afr);
                }
            }
        }
        av_packet_unref(pkt);
    }

    if (threaded) {                                   /* the interrupt callback ends a read in progress */
        pthread_mutex_lock(&q_mu); rd_quit = 1; pthread_cond_signal(&q_cv); pthread_mutex_unlock(&q_mu);
        pthread_join(rd_th, NULL);
    }
    while (fifo_n) { av_packet_free(&fifo[fifo_head]); fifo_head = (fifo_head + 1) % FIFO_MAX; fifo_n--; }
    printf("stopped after %.1f s: %ld video frames (%ld failed), %ld audio packets\n",
           now_s() - start, vframes, vfail, apk);
    subs_free();                                      /* subtitles and the bar */
    if (astr) SLAudio_FreeStream(astr);
    if (actx) SLAudio_FreeContext(actx);
    SLVideo_FreeStream(vstr);
    SLVideo_FreeContext(vctx);
    swr_free(&swr); avcodec_free_context(&adec); av_bsf_free(&bsf);
    av_frame_free(&afr); av_packet_free(&pkt); av_packet_free(&fpkt);
    avformat_close_input(&fmt);
    return rc;
}
