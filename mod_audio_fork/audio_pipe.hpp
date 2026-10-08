#ifndef __AUDIO_PIPE_HPP__
#define __AUDIO_PIPE_HPP__

#include <string>
#include <list>
#include <deque>
#include <atomic>
#include <mutex>
#include <queue>
#include <unordered_map>
#include <thread>
#include <memory>

#include <libwebsockets.h>

namespace drachtio {

  class AudioPipe : public std::enable_shared_from_this<AudioPipe> {
  public:
    using Ptr = std::shared_ptr<AudioPipe>;

    enum LwsState_t {
      LWS_CLIENT_IDLE,
      LWS_CLIENT_CONNECTING,
      LWS_CLIENT_CONNECTED,
      LWS_CLIENT_FAILED,
      LWS_CLIENT_DISCONNECTING,
      LWS_CLIENT_DISCONNECTED
    };
    enum NotifyEvent_t {
      CONNECT_SUCCESS,
      CONNECT_FAIL,
      CONNECTION_DROPPED,
      CONNECTION_CLOSED_GRACEFULLY,
      MESSAGE,
      BINARY
    };
    typedef void (*log_emit_function)(int level, const char *line);
    // userData is the object the session handed to the constructor. It is only valid for the duration of
    // the call, but stays alive at least that long even if the session has already let go of it, so
    // it is the safe way for the lws thread to reach state that the session side may be tearing down
    typedef void (*notifyHandler_t)(const char *sessionId, const char* bugname, NotifyEvent_t event, const char* message, const char* binary, size_t binary_len, void* userData);

    struct lws_per_vhost_data {
      struct lws_context *context;
      struct lws_vhost *vhost;
      const struct lws_protocols *protocol;
    };

    static void initialize(const char* protocolName, int loglevel, log_emit_function logger);
    static bool deinitialize();
    static bool lws_service_thread();

    // constructor: instances must be owned by a shared_ptr (std::make_shared)
    AudioPipe(const char* uuid, const char* host, unsigned int port, const char* path, int sslFlags, 
      size_t bufLen, size_t minFreespace, const char* username, const char* password, char* bugname,
      int bidirectional_audio, notifyHandler_t callback, std::shared_ptr<void> userData);
    ~AudioPipe();  

    LwsState_t getLwsState(void) { return m_state; }
    void connect(void);
    // queues text as one websocket text frame (held until connected), returns false if it exceeds
    // MAX_TEXT_LEN, the queue is full (message count or total bytes), or the pipe is no longer usable
    bool enqueueText(const char* text);
    size_t binarySpaceAvailable(void) {
      return m_audio_buffer_max_len - m_audio_buffer_write_offset;
    }
    size_t binaryMinSpace(void) {
      return m_audio_buffer_min_freespace;
    }
    char * binaryWritePtr(void) { 
      return (char *) m_audio_buffer + m_audio_buffer_write_offset;
    }
    void binaryWritePtrAdd(size_t len) {
      m_audio_buffer_write_offset += len;
    }
    void binaryWritePtrResetToZero(void) {
      m_audio_buffer_write_offset = 0;
    }
    void lockAudioBuffer(void) {
      m_audio_mutex.lock();
    }
    void unlockAudioBuffer(void) ;
    bool hasBasicAuth(void) {
      return !m_username.empty() && !m_password.empty();
    }

    void getBasicAuth(std::string& username, std::string& password) {
      username = m_username;
      password = m_password;
    }

    void do_graceful_shutdown();
    bool isGracefulShutdown(void) {
      return m_gracefulShutdown;
    }

    bool is_bidirectional_audio_stream() {
      return m_bidirectional_audio_stream;
    }

    void close() ;

    // no default constructor or copying
    AudioPipe() = delete;
    AudioPipe(const AudioPipe&) = delete;
    void operator=(const AudioPipe&) = delete;

  private:
    static std::thread serviceThread;

    static int lws_callback(struct lws *wsi, enum lws_callback_reasons reason, void *user, void *in, size_t len); 
    // The lws context is created by the service thread (lws_service_thread), but addPendingConnect()
    // reads it from whichever thread starts a call. Atomic so that read is not a data race.
    // It is null until the service thread has created the context (a call started right after module load
    // can see null), and it is reset to null by the service thread before the context is destroyed on unload.
    static std::atomic<struct lws_context*> context;
    static std::string protocolName;
    static std::mutex mutex_connects;
    static std::mutex mutex_disconnects;
    static std::mutex mutex_writes;
    // the pending lists and the lws per-connection slot own references, so a pipe stays alive until
    // every queued entry has been processed, regardless of when the session side lets go of it
    static std::list<Ptr> pendingConnects;
    static std::list<Ptr> pendingDisconnects;
    static std::list<Ptr> pendingWrites;
    static log_emit_function logger;

    static std::mutex mapMutex;
    // Module-wide, not per call: asks the lws service thread to exit its loop. Cleared by initialize()
    // (module load), set by deinitialize() (module unload), and checked by lws_service_thread() each time
    // lws_service() returns. Setting it does not wake lws_service(), so deinitialize() also calls lws_cancel_service().
    // Atomic because it is written by the unloading thread and read by the service thread.
    static std::atomic<bool> stopFlag;

    static Ptr findAndRemovePendingConnect(struct lws *wsi);
    static Ptr findPendingConnect(struct lws *wsi);
    static void addPendingConnect(Ptr ap);
    static void addPendingDisconnect(Ptr ap);
    static void addPendingWrite(Ptr ap);
    static void processPendingConnects(lws_per_vhost_data *vhd);
    static void processPendingDisconnects(lws_per_vhost_data *vhd);
    static void processPendingWrites(void);
    
    bool connect_client(struct lws_per_vhost_data *vhd);
    void notify(NotifyEvent_t event, const char* message, const char* binary, size_t len) {
      m_callback(m_uuid.c_str(), m_bugname.c_str(), event, message, binary, len, m_userData.get());
    }

    std::atomic<LwsState_t> m_state;
    std::atomic<bool> m_closeRequested{false};
    std::string m_uuid;
    std::string m_host;
    std::string m_bugname;
    unsigned int m_port;
    std::string m_path;
    std::deque<std::string> m_text_queue;
    std::mutex m_text_mutex;
    std::mutex m_audio_mutex;
    int m_sslFlags;
    struct lws *m_wsi;
    uint8_t *m_audio_buffer;
    size_t m_audio_buffer_max_len;
    size_t m_audio_buffer_write_offset;
    size_t m_audio_buffer_min_freespace;
    uint8_t* m_recv_buf;
    uint8_t* m_recv_buf_ptr;
    size_t m_recv_buf_len;
    struct lws_per_vhost_data* m_vhd;
    notifyHandler_t m_callback;
    // shared with the session; released with the pipe, i.e. after the last queued entry and lws callback is done
    std::shared_ptr<void> m_userData;
    log_emit_function m_logger;
    std::string m_username;
    std::string m_password;
    bool m_gracefulShutdown;
    bool m_bidirectional_audio_stream;
  };

} // namespace drachtio

#endif
