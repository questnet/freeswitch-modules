#include <switch.h>
#include <switch_json.h>
#include <string.h>
#include <string>
#include <mutex>
#include <atomic>
#include <vector>
#include <thread>
#include <list>
#include <memory>
#include <algorithm>
#include <functional>
#include <cassert>
#include <cstdlib>
#include <sstream>
#include <regex>

#include "parser.hpp"
#include "mod_audio_fork.h"
#include "audio_pipe.hpp"
#include "vector_math.h"

#include <boost/circular_buffer.hpp>


typedef boost::circular_buffer<uint16_t> CircularBuffer_t;

#define RTP_PACKETIZATION_PERIOD 20
#define FRAME_SIZE_8000  320 /*which means each 20ms frame as 320 bytes at 8 khz (1 channel only)*/
#define BUFFER_GROW_SIZE (16384)

namespace {
  static const char *requestedBufferSecs = std::getenv("MOD_AUDIO_FORK_BUFFER_SECS");
  static int nAudioBufferSecs = std::max(1, std::min(requestedBufferSecs ? ::atoi(requestedBufferSecs) : 2, 5));
  static const char *requestedNumServiceThreads = std::getenv("MOD_AUDIO_FORK_SERVICE_THREADS");
  static const char* mySubProtocolName = std::getenv("MOD_AUDIO_FORK_SUBPROTOCOL_NAME") ?
    std::getenv("MOD_AUDIO_FORK_SUBPROTOCOL_NAME") : "audio.drachtio.org";
  static unsigned int nServiceThreads = std::max(1, std::min(requestedNumServiceThreads ? ::atoi(requestedNumServiceThreads) : 1, 5));
  static unsigned int idxCallCount = 0;

  // Session-side owner of an AudioPipe. private_t is a plain C struct shared with mod_audio_fork.c,
  // so the reference lives behind its void* pAudioPipe. It is held until destroy_tech_pvt; a pipe whose
  // connection has ended is detected by its state, not by a null pointer.
  namespace PipeHandle {
    typedef std::shared_ptr<drachtio::AudioPipe> Ptr;

    void set(private_t* tech_pvt, Ptr pipe) { tech_pvt->pAudioPipe = new Ptr(std::move(pipe)); }

    drachtio::AudioPipe* get(const private_t* tech_pvt) {
      return tech_pvt->pAudioPipe ? static_cast<Ptr*>(tech_pvt->pAudioPipe)->get() : nullptr;
    }

    void release(private_t* tech_pvt) {
      delete static_cast<Ptr*>(tech_pvt->pAudioPipe);
      tech_pvt->pAudioPipe = nullptr;
    }
  }

  // Everything the lws thread touches for bidirectional audio. It is shared (shared_ptr) between the session
  // (tech_pvt->pStream) and the AudioPipe, which passes it to every callback. So cleanup on the session side
  // only drops its reference and the state stays alive until the pipe is finished with it, i.e. it is freed
  // by whoever lets go last instead of by fork_session_cleanup while the lws thread may still be using it.
  struct StreamState {
    // lws thread only
    CircularBuffer_t preBuffer{8192};
    SpeexResamplerState* resampler = nullptr;
    uint8_t setAsideByte = 0;
    bool hasSetAsideByte = false;
    int preBufSize = 0;
    int downscaleFactor = 1;

    // shared between the lws thread (insert) and the media thread (drain)
    std::mutex playoutMutex;
    CircularBuffer_t playout{8192};   // guarded by playoutMutex
    std::atomic<bool> clearPlayout{false};

    ~StreamState() {
      if (resampler) speex_resampler_destroy(resampler);
    }
  };

  // session-side owner of a StreamState reference, see PipeHandle
  namespace StreamHandle {
    typedef std::shared_ptr<StreamState> Ptr;

    void set(private_t* tech_pvt, Ptr stream) { tech_pvt->pStream = new Ptr(std::move(stream)); }

    StreamState* get(const private_t* tech_pvt) {
      return tech_pvt->pStream ? static_cast<Ptr*>(tech_pvt->pStream)->get() : nullptr;
    }

    void release(private_t* tech_pvt) {
      delete static_cast<Ptr*>(tech_pvt->pStream);
      tech_pvt->pStream = nullptr;
    }
  }

  switch_status_t processIncomingBinary(StreamState* ss, const char* message, size_t dataLength) {
    std::vector<uint8_t> data;

    // Prepend the set-aside byte if there is one
    if (ss->hasSetAsideByte) {
        data.push_back(ss->setAsideByte);
        ss->hasSetAsideByte = false;
    }

    // Append the new incoming message
    data.insert(data.end(), message, message + dataLength);

    // Check if the total data length is now odd
    if (data.size() % 2 != 0) {
        // Set aside the last byte
        ss->setAsideByte = data.back();
        ss->hasSetAsideByte = true;
        data.pop_back(); // Remove the last byte from the data vector
    }

    // Convert the data to 16-bit elements
    const uint16_t* data_uint16 = reinterpret_cast<const uint16_t*>(data.data());
    size_t numSamples = data.size() / sizeof(uint16_t);

    // Access the prebuffer
    CircularBuffer_t* cBuffer = &ss->preBuffer;

    // Ensure the prebuffer has enough capacity
    if (cBuffer->capacity() - cBuffer->size() < numSamples) {
        size_t newCapacity = cBuffer->size() + std::max(numSamples, (size_t)BUFFER_GROW_SIZE);
        cBuffer->set_capacity(newCapacity);
    }

    // Append the data to the prebuffer
    cBuffer->insert(cBuffer->end(), data_uint16, data_uint16 + numSamples);
    //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Appended %zu 16-bit samples to the prebuffer.\n", numSamples);

    // if we haven't reached threshold amount of prebuffered data, return
    if (cBuffer->size() < ss->preBufSize) {
        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Prebuffered data is below threshold %u, returning.\n", tech_pvt->streamingPreBufSize);
        return SWITCH_STATUS_SUCCESS;
    }

    //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Prebuffered data samples %u is above threshold %u, prepare to playout.\n", cBuffer->size(), tech_pvt->streamingPreBufSize);

    // after initial pre-buffering, rachet down the threshold to 40ms
    ss->preBufSize = 320 * ss->downscaleFactor * 2;

    // Check for downsampling factor
    size_t downsample_factor = ss->downscaleFactor;

    // Calculate the number of samples that can be evenly divided by the downsample factor
    size_t numCompleteSamples = (cBuffer->size() / downsample_factor) * downsample_factor;

    // Handle leftover samples
    std::vector<uint16_t> leftoverSamples;
    size_t numLeftoverSamples = cBuffer->size() - numCompleteSamples;
    if (numLeftoverSamples > 0) {
        leftoverSamples.assign(cBuffer->end() - numLeftoverSamples, cBuffer->end());
        cBuffer->resize(numCompleteSamples);
        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Temporarily removing %u leftover samples due to downsampling.\n", numLeftoverSamples);
    }

    // resample if necessary
    std::vector<int16_t> out;
    try {
      if (ss->resampler) {
        // Improvement: Use assign to convert circular buffer to vector for resampling
        std::vector<int16_t> in;
        in.assign(cBuffer->begin(), cBuffer->end()); 
        out.resize(in.size() * 6); // max upsampling would be from 8k -> 48k

        spx_uint32_t in_len = in.size();
        spx_uint32_t out_len = out.size();

        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Resampling %u samples into a buffer that can hold %u samples\n", in.size(), out_len);

        speex_resampler_process_interleaved_int(ss->resampler, in.data(), &in_len, out.data(), &out_len);

        // Resize the output buffer to match the output length from resampler
        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Resizing output buffer from %u to %u samples\n", in.size(), out_len);

        out.resize(out_len);
      }
      else {
        // If no resampling is needed, copy the data from the prebuffer to the output buffer
        out.assign(cBuffer->begin(), cBuffer->end());
      }
    } catch (const std::exception& e) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "Error resampling incoming binary message: %s\n", e.what());
      return SWITCH_STATUS_FALSE;
    } catch (...) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "Error resampling incoming binary message\n");
      return SWITCH_STATUS_FALSE;
    }

    {
      // only held for the insert; the media thread holds it just as briefly while draining
      std::lock_guard<std::mutex> lk(ss->playoutMutex);
      CircularBuffer_t *playoutBuffer = &ss->playout;

      try {
        // Resize the buffer if necessary
        if (playoutBuffer->capacity() - playoutBuffer->size() < out.size()) {
          size_t newCapacity = playoutBuffer->size() + std::max(out.size(), (size_t)BUFFER_GROW_SIZE);
          playoutBuffer->set_capacity(newCapacity);
          //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Resized playout buffer to new capacity: %zu\n", newCapacity);
        }
        // Push the data into the buffer.
        playoutBuffer->insert(playoutBuffer->end(), out.begin(), out.end());
        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Appended %zu 16-bit samples to the playout buffer.\n", out.size());
      } catch (const std::exception& e) {
        cBuffer->clear();
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "Error processing incoming binary message: %s\n", e.what());
        return SWITCH_STATUS_FALSE;
      } catch (...) {
        cBuffer->clear();
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "Error processing incoming binary message\n");
        return SWITCH_STATUS_FALSE;
      }
    }
    cBuffer->clear();

    // Put the leftover samples back in the prebuffer for the next time
    if (!leftoverSamples.empty()) {
        cBuffer->insert(cBuffer->end(), leftoverSamples.begin(), leftoverSamples.end());
        //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "Put back %u leftover samples into the prebuffer.\n", leftoverSamples.size());
    }
    return SWITCH_STATUS_SUCCESS;
  }

  void processIncomingMessage(private_t* tech_pvt, switch_core_session_t* session, const char* message, StreamState* ss) {
    std::string msg = message;
    std::string type;
    cJSON* json = parse_json(session, msg, type) ;
    if (json) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "(%u) processIncomingMessage - received %s message %s\n", tech_pvt->id, type.c_str(), message);
      cJSON* jsonData = cJSON_GetObjectItem(json, "data");
      if (0 == type.compare("killAudio")) {
        tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_KILL_AUDIO, NULL);

        // kill any current playback on the channel
        switch_channel_t *channel = switch_core_session_get_channel(session);
        switch_channel_set_flag_value(channel, CF_BREAK, 2);

        // this will dump buffered incoming audio
        ss->clearPlayout = true;
      }
      else if (0 == type.compare("transfer")) {
        char* jsonString = cJSON_PrintUnformatted(jsonData);
        tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_TRANSFER, jsonString);
        free(jsonString);                
      }
      else if (0 == type.compare("disconnect")) {
        char* jsonString = cJSON_PrintUnformatted(jsonData);
        tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_DISCONNECT, jsonString);
        free(jsonString);        
      }
      else if (0 == type.compare("error")) {
        char* jsonString = cJSON_PrintUnformatted(jsonData);
        tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_ERROR, jsonString);
        free(jsonString);        
      }
      else if (0 == type.compare("json")) {
        char* jsonString = cJSON_PrintUnformatted(json);
        tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_JSON, jsonString);
        free(jsonString);
      }
      else {
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "(%u) processIncomingMessage - unsupported msg type %s\n", tech_pvt->id, type.c_str());  
      }
      cJSON_Delete(json);
    }
    else {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "(%u) processIncomingMessage - could not parse message: %s\n", tech_pvt->id, message);
    }
  }

  static void eventCallback(const char* sessionId, const char* bugname, drachtio::AudioPipe::NotifyEvent_t event, const char* message, const char* binary, size_t len, void* userData) {
    StreamState* ss = static_cast<StreamState*>(userData);
    switch_core_session_t* session = switch_core_session_locate(sessionId);
    if (session) {
      switch_channel_t *channel = switch_core_session_get_channel(session);
      switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
      if (bug) {
        private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
        if (tech_pvt) {
          switch (event) {
            case drachtio::AudioPipe::CONNECT_SUCCESS:
              switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_INFO, "connection successful\n");
              tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_CONNECT_SUCCESS, NULL);
            break;
            case drachtio::AudioPipe::CONNECT_FAIL:
            {
              std::stringstream json;
              json << "{\"reason\":\"" << message << "\"}";
              tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_CONNECT_FAIL, (char *) json.str().c_str());
              switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_NOTICE, "connection failed: %s\n", message);
            }
            break;
            case drachtio::AudioPipe::CONNECTION_DROPPED:
              tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_DISCONNECT, NULL);
              switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_NOTICE, "connection dropped from far end\n");
            break;
            case drachtio::AudioPipe::CONNECTION_CLOSED_GRACEFULLY:
              switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "connection closed gracefully\n");
            break;
            case drachtio::AudioPipe::MESSAGE:
              processIncomingMessage(tech_pvt, session, message, ss);
            break;
            case drachtio::AudioPipe::BINARY:
            processIncomingBinary(ss, binary, len);
            break;
          }
        }
      }
      switch_core_session_rwunlock(session);
    }
  }
  switch_status_t fork_data_init(private_t *tech_pvt, switch_core_session_t *session, char * host, 
    unsigned int port, char* path, int sslFlags, int sampling, int desiredSampling, int channels, 
    char *bugname, char* metadata, int bidirectional_audio_enable,
    int bidirectional_audio_stream, int bidirectional_audio_sample_rate, responseHandler_t responseHandler) {

    const char* username = nullptr;
    const char* password = nullptr;
    int err;
    // must match the condition under which start sets SMBF_WRITE_REPLACE, otherwise nothing drains the playout buffer
    int bidirectional_audio_stream_enable =
      bidirectional_audio_enable && bidirectional_audio_stream && bidirectional_audio_sample_rate;
    switch_codec_implementation_t read_impl;
    switch_channel_t *channel = switch_core_session_get_channel(session);

    switch_core_session_get_read_impl(session, &read_impl);
  
    if (username = switch_channel_get_variable(channel, "MOD_AUDIO_BASIC_AUTH_USERNAME")) {
      password = switch_channel_get_variable(channel, "MOD_AUDIO_BASIC_AUTH_PASSWORD");
    }

    memset(tech_pvt, 0, sizeof(private_t));
  
    tech_pvt->sessionId = switch_core_session_strdup(session, switch_core_session_get_uuid(session));
    tech_pvt->sampling = desiredSampling;
    tech_pvt->responseHandler = responseHandler;
    tech_pvt->channels = channels;
    tech_pvt->id = ++idxCallCount;
    tech_pvt->buffer_overrun_notified = 0;
    tech_pvt->audio_paused = 0;
    tech_pvt->graceful_shutdown = 0;
    StreamHandle::Ptr stream = std::make_shared<StreamState>();
    tech_pvt->bidirectional_audio_enable = bidirectional_audio_enable;
    tech_pvt->bidirectional_audio_stream = bidirectional_audio_stream;
    tech_pvt->bidirectional_audio_sample_rate = bidirectional_audio_sample_rate;
    if (bidirectional_audio_sample_rate > sampling) {
      stream->downscaleFactor = bidirectional_audio_sample_rate / sampling;
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "downscale_factor is %d\n", stream->downscaleFactor);
    }
    stream->preBufSize = 320 * stream->downscaleFactor * 4; // min 80ms prebuffer

    tech_pvt->bugname = switch_core_session_strdup(session, bugname);
    
    size_t buflen = LWS_PRE + (FRAME_SIZE_8000 * desiredSampling / 8000 * channels * 1000 / RTP_PACKETIZATION_PERIOD * nAudioBufferSecs);

    PipeHandle::Ptr ap = std::make_shared<drachtio::AudioPipe>(tech_pvt->sessionId, host, port, path, sslFlags, 
      buflen, read_impl.decoded_bytes_per_packet, username, password, bugname, bidirectional_audio_stream_enable, eventCallback, stream);

    if (metadata && *metadata && !ap->enqueueText(metadata)) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "metadata exceeds max length of %d bytes\n", MAX_TEXT_LEN);
      return SWITCH_STATUS_FALSE;
    }

    PipeHandle::set(tech_pvt, ap);
    StreamHandle::set(tech_pvt, stream);

    switch_mutex_init(&tech_pvt->mutex, SWITCH_MUTEX_NESTED, switch_core_session_get_pool(session));

    if (desiredSampling != sampling) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "(%u) resampling from %u to %u\n", tech_pvt->id, sampling, desiredSampling);
      tech_pvt->resampler = speex_resampler_init(channels, sampling, desiredSampling, SWITCH_RESAMPLE_QUALITY, &err);
      if (0 != err) {
        switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "Error initializing resampler: %s.\n", speex_resampler_strerror(err));
        return SWITCH_STATUS_FALSE;
      }
    }
    else {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "(%u) no resampling needed for this call\n", tech_pvt->id);
    }

    if (bidirectional_audio_sample_rate && sampling != bidirectional_audio_sample_rate) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "(%u) bidirectional audio resampling from %u to %u, channels %d\n", tech_pvt->id, bidirectional_audio_sample_rate, sampling, channels);
      stream->resampler = speex_resampler_init(1, bidirectional_audio_sample_rate, sampling, SWITCH_RESAMPLE_QUALITY, &err);
      if (0 != err) {
        switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "Error initializing bidirectional audio resampler: %s.\n", speex_resampler_strerror(err));
        return SWITCH_STATUS_FALSE;
      }
    }

    switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "(%u) fork_data_init\n", tech_pvt->id);

    return SWITCH_STATUS_SUCCESS;
  }

  void destroy_tech_pvt(private_t* tech_pvt) {
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_INFO, "%s (%u) destroy_tech_pvt\n", tech_pvt->sessionId, tech_pvt->id);
    // drop the session's references; the pipe and the stream state are freed once the lws side is done with them too
    PipeHandle::release(tech_pvt);
    StreamHandle::release(tech_pvt);
    if (tech_pvt->resampler) {
      speex_resampler_destroy(tech_pvt->resampler);
      tech_pvt->resampler = nullptr;
    }
    // tech_pvt->mutex is not destroyed: it lives in the session pool, and a second fork_session_cleanup
    // may still be waiting on it to find out that the first one is done
  }

  void lws_logger(int level, const char *line) {
    switch_log_level_t llevel = SWITCH_LOG_DEBUG;

    switch (level) {
      case LLL_ERR: llevel = SWITCH_LOG_ERROR; break;
      case LLL_WARN: llevel = SWITCH_LOG_WARNING; break;
      case LLL_NOTICE: llevel = SWITCH_LOG_NOTICE; break;
      case LLL_INFO: llevel = SWITCH_LOG_INFO; break;
      break;
    }
	  switch_log_printf(SWITCH_CHANNEL_LOG, llevel, "%s\n", line);
  }
}

extern "C" {
  int parse_ws_uri(switch_channel_t *channel, const char* szServerUri, char* host, char *path, unsigned int* pPort, int* pSslFlags) {
    int i = 0, offset;
    char *saveptr;
    int flags = LCCSCF_USE_SSL;
    
    if (switch_true(switch_channel_get_variable(channel, "MOD_AUDIO_FORK_ALLOW_SELFSIGNED"))) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "parse_ws_uri - allowing self-signed certs\n");
      flags |= LCCSCF_ALLOW_SELFSIGNED;
    }
    if (switch_true(switch_channel_get_variable(channel, "MOD_AUDIO_FORK_SKIP_SERVER_CERT_HOSTNAME_CHECK"))) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "parse_ws_uri - skipping hostname check\n");
      flags |= LCCSCF_SKIP_SERVER_CERT_HOSTNAME_CHECK;
    }
    if (switch_true(switch_channel_get_variable(channel, "MOD_AUDIO_FORK_ALLOW_EXPIRED"))) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "parse_ws_uri - allowing expired certs\n");
      flags |= LCCSCF_ALLOW_EXPIRED;
    }

    // get the scheme
    const char *server = szServerUri;
    if (0 == strncmp(server, "https://", 8) || 0 == strncmp(server, "HTTPS://", 8)) {
      *pSslFlags = flags;
      offset = 8;
      *pPort = 443;
    }
    else if (0 == strncmp(server, "wss://", 6) || 0 == strncmp(server, "WSS://", 6)) {
      *pSslFlags = flags;
      offset = 6;
      *pPort = 443;
    }
    else if (0 == strncmp(server, "http://", 7) || 0 == strncmp(server, "HTTP://", 7)) {
      offset = 7;
      *pSslFlags = 0;
      *pPort = 80;
    }
    else if (0 == strncmp(server, "ws://", 5) || 0 == strncmp(server, "WS://", 5)) {
      offset = 5;
      *pSslFlags = 0;
      *pPort = 80;
    }
    else {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "parse_ws_uri - error parsing uri %s: invalid scheme\n", szServerUri);;
      return 0;
    }

    std::string strHost(server + offset);
    //- `([^/:]+)` captures the hostname/IP address, match any character except in the set
    //- `:?([0-9]*)?` optionally captures a colon and the port number, if it's present.
    //- `(/.*)` captures everything else (the path).
    std::regex re("([^/:]+):?([0-9]*)?(/.*)?$");
    std::smatch matches;
    if(std::regex_search(strHost, matches, re)) {
      /*
      for (int i = 0; i < matches.length(); i++) {
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "parse_ws_uri - %d: %s\n", i, matches[i].str().c_str());
      }
      */
      const std::string strMatchedHost = matches[1].str();
      const std::string strMatchedPath = matches[3].str();
      if (strMatchedHost.length() >= MAX_WS_URL_LEN || strMatchedPath.length() >= MAX_PATH_LEN) {
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "parse_ws_uri - host or path too long\n");
        return 0;
      }
      switch_copy_string(host, strMatchedHost.c_str(), MAX_WS_URL_LEN);
      if (matches[2].str().length() > 0) {
        *pPort = atoi(matches[2].str().c_str());
      }
      if (matches[3].str().length() > 0) {
        switch_copy_string(path, strMatchedPath.c_str(), MAX_PATH_LEN);
      }
      else {
        switch_copy_string(path, "/", MAX_PATH_LEN);
      }
    } else {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "parse_ws_uri - invalid format %s\n", strHost.c_str());
      return 0;
    }
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "parse_ws_uri - host %s, path %s\n", host, path);

    return 1;
  }

  switch_status_t fork_init() {
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "mod_audio_fork: audio buffer (in secs):    %d secs\n", nAudioBufferSecs);
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "mod_audio_fork: sub-protocol:              %s\n", mySubProtocolName);
 
    //int logs = LLL_ERR | LLL_WARN | LLL_NOTICE | LLL_INFO | LLL_PARSER | LLL_HEADER | LLL_EXT | LLL_CLIENT  | LLL_LATENCY | LLL_DEBUG ;
    int logs = LLL_ERR | LLL_WARN | LLL_NOTICE;
    drachtio::AudioPipe::initialize(mySubProtocolName, logs, lws_logger);
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "mod_audio_fork successfully initialized\n");
    return SWITCH_STATUS_SUCCESS;
  }

  switch_status_t fork_cleanup() {
    bool cleanup = false;
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "mod_audio_fork unloading..\n");

    cleanup = drachtio::AudioPipe::deinitialize();
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "mod_audio_fork unloaded status %d\n", cleanup);
    if (cleanup == true) {
        return SWITCH_STATUS_SUCCESS;
    }
    return SWITCH_STATUS_FALSE;
  }

  switch_status_t fork_session_init(switch_core_session_t *session, 
    responseHandler_t responseHandler,
    uint32_t samples_per_second, 
    char *host,
    unsigned int port,
    char *path,
    int sampling,
    int sslFlags,
    int channels,
    char *bugname,
    char* metadata,
    int bidirectional_audio_enable,
    int bidirectional_audio_stream,
    int bidirectional_audio_sample_rate,
    void **ppUserData
    )
  {
    int err;

    // allocate per-session data structure
    private_t* tech_pvt = (private_t *) switch_core_session_alloc(session, sizeof(private_t));
    if (!tech_pvt) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "error allocating memory!\n");
      return SWITCH_STATUS_FALSE;
    }

    if (SWITCH_STATUS_SUCCESS != fork_data_init(tech_pvt, session, host, port, path, sslFlags, samples_per_second, sampling, channels, 
      bugname, metadata, bidirectional_audio_enable, bidirectional_audio_stream, bidirectional_audio_sample_rate, responseHandler)) {
      destroy_tech_pvt(tech_pvt);
      return SWITCH_STATUS_FALSE;
    }

    *ppUserData = tech_pvt;
    return SWITCH_STATUS_SUCCESS;
  }

  // for a session that was initialised but never got as far as having a media bug (or a connection) to clean up
  void fork_session_destroy(void **ppUserData) {
    private_t *tech_pvt = static_cast<private_t *>(*ppUserData);
    if (tech_pvt) destroy_tech_pvt(tech_pvt);
    *ppUserData = nullptr;
  }

   switch_status_t fork_session_connect(void **ppUserData) {
    private_t *tech_pvt = static_cast<private_t *>(*ppUserData);
    drachtio::AudioPipe *pAudioPipe = PipeHandle::get(tech_pvt);
    if (!pAudioPipe) return SWITCH_STATUS_FALSE;
    pAudioPipe->connect();
    return SWITCH_STATUS_SUCCESS;
  }

  switch_status_t fork_session_cleanup(switch_core_session_t *session, char *bugname, char* text, int channelIsClosing) {
    switch_channel_t *channel = switch_core_session_get_channel(session);
    switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "fork_session_cleanup: no bug %s - websocket conection already closed\n", bugname);
      return SWITCH_STATUS_FALSE;
    }
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
    uint32_t id = tech_pvt->id;

    switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_DEBUG, "(%u) fork_session_cleanup\n", id);

    if (!tech_pvt) return SWITCH_STATUS_FALSE;

    switch_mutex_lock(tech_pvt->mutex);

    // get the bug again, now that we are under lock. stop and the media bug close can both get here
    // (a close can even be triggered by the bug removal below); only the first one still finds the bug
    // and tears down, any other returns here.
    bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_mutex_unlock(tech_pvt->mutex);
      return SWITCH_STATUS_FALSE;
    }
    switch_channel_set_private(channel, bugname, NULL);
    if (!channelIsClosing) {
      switch_core_media_bug_remove(session, &bug);
    }
    drachtio::AudioPipe *pAudioPipe = PipeHandle::get(tech_pvt);

    // A final text is only worth sending on a live connection. Checking the state and then queueing is
    // not atomic: the state may change in between, e.g. the connection drops. That is harmless, enqueueText
    // re-checks the state under the queue lock and either queues the text (it is freed with the pipe
    // if never sent) or rejects it, and we ignore the result because the pipe is closed right after.
    if (pAudioPipe && text && pAudioPipe->getLwsState() == drachtio::AudioPipe::LWS_CLIENT_CONNECTED) pAudioPipe->enqueueText(text);
    if (pAudioPipe) pAudioPipe->close();

    destroy_tech_pvt(tech_pvt);
    switch_mutex_unlock(tech_pvt->mutex);
    switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_INFO, "(%u) fork_session_cleanup: connection closed\n", id);
    return SWITCH_STATUS_SUCCESS;
  }

  switch_status_t fork_session_send_text(switch_core_session_t *session, char *bugname, char* text) {
    switch_channel_t *channel = switch_core_session_get_channel(session);
    switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "fork_session_send_text failed because no bug\n");
      return SWITCH_STATUS_FALSE;
    }
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
  
    if (!tech_pvt) return SWITCH_STATUS_FALSE;
    // the pipe handle is released under this mutex by fork_session_cleanup (null afterwards)
    switch_mutex_lock(tech_pvt->mutex);
    drachtio::AudioPipe *pAudioPipe = PipeHandle::get(tech_pvt);
    bool failed = pAudioPipe && text && !pAudioPipe->enqueueText(text);
    switch_mutex_unlock(tech_pvt->mutex);

    if (failed) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "fork_session_send_text failed: text exceeds max length of %d bytes, send queue is full, or connection is closed\n", MAX_TEXT_LEN);
      return SWITCH_STATUS_FALSE;
    }

    return SWITCH_STATUS_SUCCESS;
  }

  switch_status_t fork_session_pauseresume(switch_core_session_t *session, char *bugname, int pause) {
    switch_channel_t *channel = switch_core_session_get_channel(session);
    switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "fork_session_pauseresume failed because no bug\n");
      return SWITCH_STATUS_FALSE;
    }
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
  
    if (!tech_pvt) return SWITCH_STATUS_FALSE;

    switch_core_media_bug_flush(bug);
    tech_pvt->audio_paused = pause;
    return SWITCH_STATUS_SUCCESS;
  }

  switch_status_t fork_session_graceful_shutdown(switch_core_session_t *session, char *bugname) {
    switch_channel_t *channel = switch_core_session_get_channel(session);
    switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "fork_session_graceful_shutdown failed because no bug\n");
      return SWITCH_STATUS_FALSE;
    }
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
  
    if (!tech_pvt) return SWITCH_STATUS_FALSE;

    tech_pvt->graceful_shutdown = 1;

    // the pipe handle is released under this mutex by fork_session_cleanup (null afterwards)
    switch_mutex_lock(tech_pvt->mutex);
    drachtio::AudioPipe *pAudioPipe = PipeHandle::get(tech_pvt);
    if (pAudioPipe) pAudioPipe->do_graceful_shutdown();
    switch_mutex_unlock(tech_pvt->mutex);

    return SWITCH_STATUS_SUCCESS;
  }

  switch_bool_t fork_frame(switch_core_session_t *session, switch_media_bug_t *bug) {
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);
    size_t inuse = 0;
    bool dirty = false;
    char *p = (char *) "{\"msg\": \"buffer overrun\"}";

    if (!tech_pvt || tech_pvt->audio_paused || tech_pvt->graceful_shutdown) return SWITCH_TRUE;
    
    if (switch_mutex_trylock(tech_pvt->mutex) == SWITCH_STATUS_SUCCESS) {
      drachtio::AudioPipe *pAudioPipe = PipeHandle::get(tech_pvt);
      if (!pAudioPipe || pAudioPipe->getLwsState() != drachtio::AudioPipe::LWS_CLIENT_CONNECTED) {
        switch_mutex_unlock(tech_pvt->mutex);
        return SWITCH_TRUE;
      }

      pAudioPipe->lockAudioBuffer();
      size_t available = pAudioPipe->binarySpaceAvailable();
      if (NULL == tech_pvt->resampler) {
        switch_frame_t frame = { 0 };
        frame.data = pAudioPipe->binaryWritePtr();
        frame.buflen = available;
        while (true) {

          // check if buffer would be overwritten; dump packets if so
          if (available < pAudioPipe->binaryMinSpace()) {
            if (!tech_pvt->buffer_overrun_notified) {
              tech_pvt->buffer_overrun_notified = 1;
              tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_BUFFER_OVERRUN, NULL);
            }
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "(%u) dropping packets!\n", 
              tech_pvt->id);
            pAudioPipe->binaryWritePtrResetToZero();

            frame.data = pAudioPipe->binaryWritePtr();
            frame.buflen = available = pAudioPipe->binarySpaceAvailable();
          }

          switch_status_t rv = switch_core_media_bug_read(bug, &frame, SWITCH_TRUE);
          if (rv != SWITCH_STATUS_SUCCESS) break;
          if (frame.datalen) {
            pAudioPipe->binaryWritePtrAdd(frame.datalen);
            frame.buflen = available = pAudioPipe->binarySpaceAvailable();
            frame.data = pAudioPipe->binaryWritePtr();
            dirty = true;
          }
        }
      }
      else {
        uint8_t data[SWITCH_RECOMMENDED_BUFFER_SIZE];
        switch_frame_t frame = { 0 };
        frame.data = data;
        frame.buflen = SWITCH_RECOMMENDED_BUFFER_SIZE;
        while (switch_core_media_bug_read(bug, &frame, SWITCH_TRUE) == SWITCH_STATUS_SUCCESS) {
          if (frame.datalen) {
            spx_uint32_t out_len = available / (sizeof(spx_int16_t) * tech_pvt->channels);  // space for frames of 2 bytes per channel
            spx_uint32_t in_len = frame.samples;

            speex_resampler_process_interleaved_int(tech_pvt->resampler, 
              (const spx_int16_t *) frame.data, 
              (spx_uint32_t *) &in_len, 
              (spx_int16_t *) ((char *) pAudioPipe->binaryWritePtr()),
              &out_len);

            if (out_len > 0) {
              // bytes written = num samples * 2 * num channels
              size_t bytes_written = out_len * sizeof(spx_int16_t) * tech_pvt->channels;
              pAudioPipe->binaryWritePtrAdd(bytes_written);
              available = pAudioPipe->binarySpaceAvailable();
              dirty = true;
            }
            if (available < pAudioPipe->binaryMinSpace()) {
              if (!tech_pvt->buffer_overrun_notified) {
                tech_pvt->buffer_overrun_notified = 1;
                switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "(%u) dropping packets!\n", 
                  tech_pvt->id);
                tech_pvt->responseHandler(session, tech_pvt->bugname, EVENT_BUFFER_OVERRUN, NULL);
              }
              break;
            }
          }
        }
      }

      pAudioPipe->unlockAudioBuffer();
      switch_mutex_unlock(tech_pvt->mutex);
    }
    return SWITCH_TRUE;
  }

  // Write-replace callback of the media bug, called on the media thread for every outgoing frame (20 ms).
  // Mixes the audio streamed in from the websocket (queued in StreamState::playout by processIncomingBinary
  // on the lws thread) into the frame that is sent to the caller: it takes up to one frame of samples from
  // the playout buffer, adds them to the frame and writes the frame back. If a killAudio cleared the buffer
  // (clearPlayout), the buffered audio is dropped instead and this frame is left unchanged.
  // Never blocks and never fails: when the session mutex or the playout buffer is busy, or the stream state
  // is already released, the frame is passed on untouched and the buffered audio is played with the next frame.
  switch_bool_t dub_speech_frame(switch_media_bug_t *bug, private_t* tech_pvt) {
    if (switch_mutex_trylock(tech_pvt->mutex) == SWITCH_STATUS_SUCCESS) {
      // the stream state is released under tech_pvt->mutex by cleanup, so it is read under it here
      StreamState* ss = StreamHandle::get(tech_pvt);
      // like the session lock, the playout buffer is only tried: skip this frame rather than wait for the lws thread
      std::unique_lock<std::mutex> playoutLock;
      if (ss) playoutLock = std::unique_lock<std::mutex>(ss->playoutMutex, std::try_to_lock);
      if (ss && playoutLock.owns_lock()) {
        CircularBuffer_t *cBuffer = &ss->playout;

        // if flag was set to clear the buffer, do so and clear the flag
        if (ss->clearPlayout.exchange(false)) {
          switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "(%u) dub_speech_frame - clearing buffer\n", tech_pvt->id); 
          cBuffer->clear();
        }
        else {
          switch_frame_t* rframe = switch_core_media_bug_get_write_replace_frame(bug);
          int16_t *fp = reinterpret_cast<int16_t*>(rframe->data);

          rframe->channels = 1;
          rframe->datalen = rframe->samples * sizeof(int16_t);

          int16_t data[SWITCH_RECOMMENDED_BUFFER_SIZE];
          memset(data, 0, sizeof(data));

          int samplesToCopy = std::min(static_cast<int>(cBuffer->size()), static_cast<int>(rframe->samples));

          //switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "(%u) dub_speech_frame - samples to copy %u\n", tech_pvt->id, samplesToCopy); 

          std::copy_n(cBuffer->begin(), samplesToCopy, data);
          cBuffer->erase(cBuffer->begin(), cBuffer->begin() + samplesToCopy);

          if (samplesToCopy > 0) {
            vector_add(fp, data, rframe->samples);
          }
          vector_normalize(fp, rframe->samples);

          switch_core_media_bug_set_write_replace_frame(bug, rframe);
        }
      }
      playoutLock = std::unique_lock<std::mutex>();
      switch_mutex_unlock(tech_pvt->mutex);
    }
    return SWITCH_TRUE;
  }

  switch_status_t fork_session_stop_play(switch_core_session_t *session, char *bugname) {
    switch_channel_t *channel = switch_core_session_get_channel(session);
    switch_media_bug_t *bug = (switch_media_bug_t*) switch_channel_get_private(channel, bugname);
    if (!bug) {
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), SWITCH_LOG_ERROR, "fork_session_stop_play failed because no bug\n");
      return SWITCH_STATUS_FALSE;
    }
    private_t* tech_pvt = (private_t*) switch_core_media_bug_get_user_data(bug);

    if (switch_mutex_trylock(tech_pvt->mutex) == SWITCH_STATUS_SUCCESS) {
      StreamState* ss = StreamHandle::get(tech_pvt);
      if (ss) {
        std::lock_guard<std::mutex> lk(ss->playoutMutex);
        ss->playout.clear();
      }
      switch_mutex_unlock(tech_pvt->mutex);
    }
    return SWITCH_STATUS_SUCCESS;
  }

}

