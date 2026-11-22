/* sljf-static: TV static (grey snow + a faint hiss) on a full-screen SLVideo overlay while a Live TV channel tunes
 * or the stream has run dry (SLJF_STATIC). See sljf-static.c. GPL, like the rest of steamlink-jellyfin. */
#ifndef SLJF_STATIC_H
#define SLJF_STATIC_H
#include <SLVideo.h>
#include <SLAudio.h>
#include <stdint.h>

#define STATIC_AUDIO_CHUNK_BYTES (48000 * 20 / 1000 * 2 * 2)   /* 20 ms of 48 kHz s16 stereo, = sljf-play's chunk */

int static_volume(void);           /* SLJF_STATIC as 0..100, or -1 when unset (no static) */
int static_show(CSLVideoContext *v, int volume);   /* put it up now; 0 ok, -1 no overlay */
void static_tick(double now_s);    /* new snow ~15/s, keeps <= 100 ms of hiss queued; call often */
void static_hide(void);
/* a card (sljf-play --channel): draw this text on the snow from the next frame on; NULL = plain static again */
void static_card(const char *text);
int static_visible(void);
/* Share one SLAudio stream with the player: when the static made its own, it is handed over (*c, *s set; the
   player frees it from then on); when the player already has one (*s), the static puts its rebuffer hiss there. */
void static_give_audio(CSLAudioContext **c, CSLAudioStream **s);
void static_prog_audio(void);      /* the player just queued programme sound: no more hiss until the next show */
/* While the main thread blocks in avformat_open_input / find_stream_info: a thread ticks instead (on = 1), and is
   stopped (on = 0) before the main thread touches SLVideo / SLAudio again. */
void static_background(int on);
void static_set_gain(int32_t g);   /* the player's volume gain (65536 = full): the hiss follows it */
void static_free(void);            /* overlays and an own audio stream; safe to call twice */
#endif
