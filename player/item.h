/* One playable item of sljf-play: its demuxer, the read-ahead FIFO and its reader thread, the bitstream filter and
   the audio decoder. main() plays one (`cur`); channel mode later plays a queue of them (episode, bumper, episode)
   into the same SLVideo/SLAudio streams, each with its own demuxer, reader and FIFO. */
#ifndef SLJF_ITEM_H
#define SLJF_ITEM_H
#include <libavformat/avformat.h>
#include <libavcodec/avcodec.h>
#include <libavcodec/bsf.h>
#include <libswresample/swresample.h>
#include <signal.h>
#include <pthread.h>

#define FIFO_MAX 4096            /* read-ahead packets (~25 s of 25 fps video + audio) */

struct item {
    AVFormatContext *fmt;
    int vi, ai;                  /* video / audio stream index (ai < 0: no sound) */
    AVBSFContext *bsf;           /* h264_mp4toannexb, or NULL when the stream is Annex-B already */
    AVCodecContext *adec;
    SwrContext *swr;
    AVPacket *fifo[FIFO_MAX];    /* packets read but not yet fed, with their stream time */
    double fifo_ts[FIFO_MAX];
    int fifo_head, fifo_n;
    double last_ts;              /* newest packet's stream time */
    int rd_eof, rd_err;          /* the reader thread saw the end / a stream error */
    volatile sig_atomic_t rd_quit;
    pthread_t rd_th;
    int threaded;                /* the reader thread runs */
    double limit_s;              /* play at most this long (0 = to the end) */
    double first_ts;             /* stream time of the first packet read */
    long pushed;                 /* packets read into the FIFO so far */
    long bytes;                  /* packet bytes in the FIFO now */
    double ahead;                /* its reader stops while this much is read ahead (channel: small while it waits) */
    int over;                    /* limit_s reached: nothing more of it is fed */
    int seq;                     /* position in the queue */
    int kind;                    /* 0 video, 1 card */
    int w, h;                    /* video size (SLVideo stream: same size = same stream) */
    double start_s;              /* where in the programme the stream starts (SLJF_START / "next v"), for the bar */
};

/* open the URL and probe its streams (blocks: the caller keeps the static moving); <0 = FFmpeg error, already
   reported on stderr. opts is not consumed: the caller frees it. */
int item_open(struct item *it, const char *url, AVDictionary *opts);
/* stop the reader thread and free everything the item holds */
void item_close(struct item *it);
#endif
