#ifndef __MOD_FORK_H__
#define __MOD_FORK_H__

#include <switch.h>
#include <libwebsockets.h>
#include <speex/speex_resampler.h>

#include <unistd.h>

#define MY_BUG_NAME "audio_fork"
#define MAX_SESSION_ID (256)
#define MAX_WS_URL_LEN (512)
#define MAX_PATH_LEN (4096)

#define EVENT_TRANSFER        "mod_audio_fork::transfer"
#define EVENT_KILL_AUDIO      "mod_audio_fork::kill_audio"
#define EVENT_DISCONNECT      "mod_audio_fork::disconnect"
#define EVENT_ERROR           "mod_audio_fork::error"
#define EVENT_CONNECT_SUCCESS "mod_audio_fork::connect"
#define EVENT_CONNECT_FAIL    "mod_audio_fork::connect_failed"
#define EVENT_BUFFER_OVERRUN  "mod_audio_fork::buffer_overrun"
#define EVENT_JSON            "mod_audio_fork::json"

// Max length in bytes of the start metadata and of each send_text message.
// Both used to be limited/unbounded by accident: the start metadata was copied into a fixed
// 8K char array (silently truncated), and send_text text was unbounded and copied into a
// stack array when written, which could overflow the stack for large messages.
// Now both are held on the heap and rejected (never truncated, as truncated JSON is invalid)
// when longer than this limit.
#define MAX_TEXT_LEN (1024 * 1024)

typedef void (*responseHandler_t)(switch_core_session_t* session, const char* bugname, const char* eventName, char* json);

struct private_data {
	switch_mutex_t *mutex;
	char sessionId[MAX_SESSION_ID];
  const char *bugname;
  SpeexResamplerState *resampler;
  responseHandler_t responseHandler;
  void *pAudioPipe;
  int ws_state;
  char host[MAX_WS_URL_LEN];
  unsigned int port;
  char path[MAX_PATH_LEN];
  int sampling;
  int  channels;
  unsigned int id;
  int buffer_overrun_notified:1;
  int audio_paused:1;
  int graceful_shutdown:1;

  // bidirectional audio
  // session-side reference to the shared StreamState (see lws_glue.cpp); the lws thread reaches the same
  // object through the AudioPipe, so releasing this at cleanup does not free it under the lws thread
  void *pStream;
  int bidirectional_audio_enable;
	int bidirectional_audio_stream;
  int bidirectional_audio_sample_rate;
};

typedef struct private_data private_t;

#endif
