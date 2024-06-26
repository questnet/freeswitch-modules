#include <switch.h>
#include <switch_json.h>
#include <grpc++/grpc++.h>

#include "mod_google_transcribe.h"
#include "gstreamer.h"
#include "generic_google_glue.h"

#include "google/cloud/speech/v2/cloud_speech.grpc.pb.h"

using google::cloud::speech::v2::RecognitionConfig;
using google::cloud::speech::v2::Speech;
using google::cloud::speech::v2::StreamingRecognizeRequest;
using google::cloud::speech::v2::StreamingRecognizeResponse;
using google::cloud::speech::v2::SpeakerDiarizationConfig;
using google::cloud::speech::v2::SpeechAdaptation;
using google::cloud::speech::v2::SpeechRecognitionAlternative;
using google::cloud::speech::v2::PhraseSet;
using google::cloud::speech::v2::PhraseSet_Phrase;
using google::cloud::speech::v2::StreamingRecognizeResponse_SpeechEventType_END_OF_SINGLE_UTTERANCE;
using google::cloud::speech::v2::StreamingRecognizeResponse_SpeechEventType_SPEECH_ACTIVITY_BEGIN;
using google::cloud::speech::v2::StreamingRecognizeResponse_SpeechEventType_SPEECH_ACTIVITY_END;
using google::cloud::speech::v2::ExplicitDecodingConfig_AudioEncoding_LINEAR16;
using google::cloud::speech::v2::RecognitionFeatures_MultiChannelMode_SEPARATE_RECOGNITION_PER_CHANNEL;
using google::cloud::speech::v2::SpeechAdaptation_AdaptationPhraseSet;
using google::rpc::Status;

typedef GStreamer<StreamingRecognizeRequest, StreamingRecognizeResponse, Speech::Stub> GStreamer_V2;

template<>
GStreamer<StreamingRecognizeRequest, StreamingRecognizeResponse, Speech::Stub>::GStreamer(
    switch_core_session_t *session, 
    uint32_t channels, 
    char* lang, 
    int interim, 
    uint32_t config_sample_rate,
	uint32_t samples_per_second,
    int single_utterance, 
    int separate_recognition,
	int max_alternatives, 
    int profanity_filter, 
    int word_time_offset, 
    int punctuation, 
    const char* model, 
    int enhanced, 
	const char* hints) : m_session(session), m_writesDone(false), m_connected(false),
    m_audioBuffer(CHUNKSIZE, 15) {
  
    switch_channel_t *channel = switch_core_session_get_channel(session);
    m_channel = create_grpc_channel(channel);
  	m_stub = Speech::NewStub(m_channel);

	auto streaming_config = m_request.mutable_streaming_config();
    const char* var;

    // The parent of the recognizer must still be provided even if the wildcard
    // recognizer is used rather than a pre-prepared recognizer.
    std::string recognizer;
    if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_RECOGNIZER_PARENT")) {
        recognizer = var;
        recognizer += "/recognizers/";
    } else {
        throw std::runtime_error("The v2 Speech-To-Text library requires GOOGLE_SPEECH_RECOGNIZER_PARENT to be set");
    }

    // Use the recognizer specified in the variable or just use the wildcard if this is not set.
    if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_RECOGNIZER_ID")) {
        recognizer += var;
    } else {
        recognizer += "_";

        RecognitionConfig* config = streaming_config->mutable_config();
        config->add_language_codes(lang);
        switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "transcribe language %s\n", lang);
        
        // alternative language
        if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_ALTERNATIVE_LANGUAGE_CODES")) {
            char *alt_langs[3] = { 0 };
            int argc = switch_separate_string((char *) var, ',', alt_langs, 3);
            for (int i = 0; i < argc; i++) {
                config->add_language_codes(alt_langs[i]);
                switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "added alternative lang %s\n", alt_langs[i]);
            }
        }

        config->mutable_explicit_decoding_config()->set_sample_rate_hertz(config_sample_rate);
        config->mutable_explicit_decoding_config()->set_encoding(ExplicitDecodingConfig_AudioEncoding_LINEAR16);

        // number of channels in the audio stream (default: 1)
        // N.B. It is essential to set this configuration value in v2 even if it doesn't deviate from the default.
        config->mutable_explicit_decoding_config()->set_audio_channel_count(channels);
        switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "audio_channel_count %d\n", channels);
        if (channels > 1) {
            // transcribe each separately?
            if (separate_recognition == 1) {
                config->mutable_features()->set_multi_channel_mode(RecognitionFeatures_MultiChannelMode_SEPARATE_RECOGNITION_PER_CHANNEL);
                switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "enable_separate_recognition_per_channel on\n");
            }
        }

        // max alternatives
        if (max_alternatives > 1) {
            config->mutable_features()->set_max_alternatives(max_alternatives);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "max_alternatives %d\n", max_alternatives);
        }

        // profanity filter
        if (profanity_filter == 1) {
            config->mutable_features()->set_profanity_filter(true);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "profanity_filter\n");
        }

        // enable word offsets
        if (word_time_offset == 1) {
            config->mutable_features()->set_enable_word_time_offsets(true);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "enable_word_time_offsets\n");
        }

        // enable automatic punctuation
        if (punctuation == 1) {
            config->mutable_features()->set_enable_automatic_punctuation(true);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "enable_automatic_punctuation\n");
        }
        else {
            config->mutable_features()->set_enable_automatic_punctuation(false);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "disable_automatic_punctuation\n");
        }

        // speech model
        if (model != NULL) {
            config->set_model(model);
            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "speech model %s\n", model);
        }

        // hints  
        if (hints != NULL) {
            auto* adaptation = config->mutable_adaptation();
            auto* phrase_set = adaptation->add_phrase_sets()->mutable_inline_phrase_set();
            google_speech_configure_grammar_hints(m_session, channel, hints, phrase_set);
        }

        // the rest of config comes from channel vars

        // speaker diarization
        // N.B. At the moment there does not seem to be any combination of model, language and location which supports diarization for STT v2.
        // See https://stackoverflow.com/questions/76779418/speaker-diarization-is-disabled-even-for-supported-languages-in-google-speech-to
        if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_SPEAKER_DIARIZATION")) {
            auto* diarization_config = config->mutable_features()->mutable_diarization_config();
            // There is no enable function in v2
            // diarization_config->set_enable_speaker_diarization(true);
            // switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "enabling speaker diarization\n", var);
            if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_SPEAKER_DIARIZATION_MIN_SPEAKER_COUNT")) {
                int count = std::max(atoi(var), 1);
                switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "setting min speaker count to %d\n", count);
                diarization_config->set_min_speaker_count(count);
            }
            if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_SPEAKER_DIARIZATION_MAX_SPEAKER_COUNT")) {
                int count = std::max(atoi(var), 2);
                switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "setting max speaker count to %d\n", count);
                diarization_config->set_max_speaker_count(count);
            }
        }
        if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_TRANSCRIPTION_NORMALIZATION")) {
          // parse JSON string
        cJSON *json_array = cJSON_Parse(var);

        int array_size = cJSON_GetArraySize(json_array);

        for(int i=0; i<array_size; i++) {
            cJSON* json_item = cJSON_GetArrayItem(json_array, i);

            auto entry = config->mutable_transcript_normalization()->add_entries();

            std::string search_string = cJSON_GetObjectItem(json_item, "search")->valuestring;
            std::string replacement_string = cJSON_GetObjectItem(json_item, "replace")->valuestring;
            bool case_sensitive = cJSON_GetObjectItem(json_item, "case_sensitive")->valueint != 0;

            entry->set_search(search_string);
            entry->set_replace(replacement_string);
            entry->set_case_sensitive(case_sensitive);

            switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG,
              "TRANSCRIPTION_NORMALIZATION search %s, replace %s, set_case_sensitive %d\n", search_string.c_str(), replacement_string.c_str(), case_sensitive);
        }
        // clean json
        cJSON_Delete(json_array);
      }
    }
    if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_START_TIMEOUT_MS")) {
      auto ms = atoi(var);
      streaming_config->mutable_streaming_features()->mutable_voice_activity_timeout()->mutable_speech_start_timeout()->set_nanos(ms * 1000000);
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "setting speech_start_timeout to %d milliseconds\n", ms);
    }

    if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_END_TIMEOUT_MS")) {
      auto ms = atoi(var);
      streaming_config->mutable_streaming_features()->mutable_voice_activity_timeout()->mutable_speech_end_timeout()->set_nanos(ms * 1000000);
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "setting speech_end_timeout to %d milliseconds\n", ms);
    }

    if (var = switch_channel_get_variable(channel, "GOOGLE_SPEECH_ENABLE_VOICE_ACTIVITY_EVENTS")) {
      bool enabled = !strcmp(var, "true") ? 1 : 0;
      streaming_config->mutable_streaming_features()->set_enable_voice_activity_events(enabled);
      switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "setting enable_voice_activity_events to %d \n", enabled);
    }

    m_request.set_recognizer(recognizer);
    switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(m_session), SWITCH_LOG_DEBUG, "using recognizer: %s\n", recognizer.c_str());

    // This must be set whether a recognizer id is provided or not, because it cannot be configured as part of a recognizer.
    if (interim > 0) {
        streaming_config->mutable_streaming_features()->set_interim_results(interim > 0);
    }
}

static void *SWITCH_THREAD_FUNC grpc_read_thread(switch_thread_t *thread, void *obj) {
  static int count;
	struct cap_cb *cb = (struct cap_cb *) obj;
	GStreamer_V2* streamer = (GStreamer_V2 *) cb->streamer;

  bool connected = streamer->waitForConnect();
  if (!connected) {
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_NOTICE, "google transcribe grpc read thread exiting since we didn't connect\n") ;
    return nullptr;
  }

  // Read responses.
  StreamingRecognizeResponse response;
  while (streamer->read(&response)) {  // Returns false when no more to read.
    switch_core_session_t* session = switch_core_session_locate(cb->sessionId);
    if (!session) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_ERROR, "grpc_read_thread: session %s is gone!\n", cb->sessionId) ;
      return nullptr;
    }
    count++;
    
    if (cb->play_file == 1){
      cb->responseHandler(session, "play_interrupt", cb->bugname);
    }
    
    for (int r = 0; r < response.results_size(); ++r) {
      auto result = response.results(r);
      cJSON * jResult = cJSON_CreateObject();
      cJSON * jAlternatives = cJSON_CreateArray();
      cJSON * jStability = cJSON_CreateNumber(result.stability());
      cJSON * jIsFinal = cJSON_CreateBool(result.is_final());
      cJSON * jLanguageCode = cJSON_CreateString(result.language_code().c_str());
      cJSON * jChannelTag = cJSON_CreateNumber(result.channel_tag());

      auto duration = result.result_end_offset();
      int32_t seconds = duration.seconds();
      int64_t nanos = duration.nanos();
      int span = (int) trunc(seconds * 1000. + ((float) nanos / 1000000.));
      cJSON * jResultEndTime = cJSON_CreateNumber(span);

      cJSON_AddItemToObject(jResult, "stability", jStability);
      cJSON_AddItemToObject(jResult, "is_final", jIsFinal);
      cJSON_AddItemToObject(jResult, "alternatives", jAlternatives);
      cJSON_AddItemToObject(jResult, "language_code", jLanguageCode);
      cJSON_AddItemToObject(jResult, "channel_tag", jChannelTag);
      cJSON_AddItemToObject(jResult, "result_end_time", jResultEndTime);

      if (result.alternatives_size() == 0) {
        SpeechRecognitionAlternative alternative;
        alternative.set_confidence(0.0);
        alternative.set_transcript("");
        *result.add_alternatives() = alternative;
      }
      for (int a = 0; a < result.alternatives_size(); ++a) {
        auto alternative = result.alternatives(a);
        cJSON* jAlt = cJSON_CreateObject();
        cJSON* jConfidence = cJSON_CreateNumber(alternative.confidence());
        cJSON* jTranscript = cJSON_CreateString(alternative.transcript().c_str());
        cJSON_AddItemToObject(jAlt, "confidence", jConfidence);
        cJSON_AddItemToObject(jAlt, "transcript", jTranscript);

        if (alternative.words_size() > 0) {
          cJSON * jWords = cJSON_CreateArray();
          switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: %d words\n", alternative.words_size()) ;
          for (int b = 0; b < alternative.words_size(); b++) {
            auto words = alternative.words(b);
            cJSON* jWord = cJSON_CreateObject();
            cJSON_AddItemToObject(jWord, "word", cJSON_CreateString(words.word().c_str()));
            if (words.has_start_offset()) {
              cJSON_AddItemToObject(jWord, "start_offset", cJSON_CreateNumber(words.start_offset().seconds()));
            }
            if (words.has_end_offset()) {
              cJSON_AddItemToObject(jWord, "end_offset", cJSON_CreateNumber(words.end_offset().seconds()));
            }
            auto speaker_label = words.speaker_label();
            if (speaker_label.size() > 0) {
              cJSON_AddItemToObject(jWord, "speaker_label", cJSON_CreateString(speaker_label.c_str()));
            }
            float confidence = words.confidence();
            if (confidence > 0.0) {
              cJSON_AddItemToObject(jWord, "confidence", cJSON_CreateNumber(confidence));
            }

            cJSON_AddItemToArray(jWords, jWord);
          }
          cJSON_AddItemToObject(jAlt, "words", jWords);
        }
        cJSON_AddItemToArray(jAlternatives, jAlt);
      }

      char* json = cJSON_PrintUnformatted(jResult);
      cb->responseHandler(session, (const char *) json, cb->bugname);
      free(json);

      cJSON_Delete(jResult);
    }

    auto speech_event_type = response.speech_event_type();
    if (speech_event_type == StreamingRecognizeResponse_SpeechEventType_END_OF_SINGLE_UTTERANCE) {
      // we only get this when we have requested it, and recognition stops after we get this
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: got end_of_utterance\n") ;
      cb->got_end_of_utterance = 1;
      cb->responseHandler(session, "end_of_utterance", cb->bugname);
      if (cb->wants_single_utterance) {
        switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: sending writesDone because we want only a single utterance\n") ;
        streamer->writesDone();
      }
    }
    else if (speech_event_type == StreamingRecognizeResponse_SpeechEventType_SPEECH_ACTIVITY_BEGIN) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: got SPEECH_ACTIVITY_BEGIN\n") ;
    }
    else if (speech_event_type == StreamingRecognizeResponse_SpeechEventType_SPEECH_ACTIVITY_END) {
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: got SPEECH_ACTIVITY_END\n") ;
    }
    switch_core_session_rwunlock(session);
    switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: got %d responses\n", response.results_size());
  }

  {
    switch_core_session_t* session = switch_core_session_locate(cb->sessionId);
    if (session) {
      grpc::Status status = streamer->finish();
      // TODO: This works on the same principle as that used in the v1 equivalent, in that we search for the textual
      // error message to determine whether the cause of the problem is the expiration of the session.
      // It would be better if we could find a more reliable way of detecting this.
      if (10 == status.error_code()) {
        if (std::string::npos != status.error_message().find("Max duration of 5 minutes reached")) {
          cb->responseHandler(session, "max_duration_exceeded", cb->bugname);
        }
        else {
          cb->responseHandler(session, "no_audio", cb->bugname);
        }
      }
      else if (status.error_code() != 0) {
        cJSON* json = cJSON_CreateObject();
        cJSON_AddStringToObject(json, "type", "error");
        cJSON_AddStringToObject(json, "error_cause", "stream_close");
        cJSON_AddItemToObject(json, "error_code", cJSON_CreateNumber(status.error_code()));
        cJSON_AddStringToObject(json, "error_message", status.error_message().c_str());
        char* jsonString = cJSON_PrintUnformatted(json);
        cb->responseHandler(session, jsonString, cb->bugname);
        free(jsonString);
        cJSON_Delete(json);
      }
      switch_log_printf(SWITCH_CHANNEL_LOG, SWITCH_LOG_DEBUG, "grpc_read_thread: finish() status %s (%d)\n", status.error_message().c_str(), status.error_code()) ;
      switch_core_session_rwunlock(session);
    }
  }
  return nullptr;
}

template <>
bool GStreamer<StreamingRecognizeRequest, StreamingRecognizeResponse, Speech::Stub>::write(void* data, uint32_t datalen) {
	if (!m_connected) {
		if (datalen % CHUNKSIZE == 0) {
			m_audioBuffer.add(data, datalen);
		}
		return true;
	}
    m_request.clear_streaming_config();
	m_request.set_audio(data, datalen);
	bool ok = m_streamer->Write(m_request);
	return ok;
}

extern "C" {

    switch_status_t google_speech_session_cleanup_v2(switch_core_session_t *session, int channelIsClosing, switch_media_bug_t *bug) {
      return google_speech_session_cleanup<GStreamer_V2>(session, channelIsClosing, bug);
    }

    switch_bool_t google_speech_frame_v2(switch_media_bug_t *bug, void* user_data) {
      return google_speech_frame<GStreamer_V2>(bug, user_data);
    }

    switch_status_t google_speech_session_init_v2(switch_core_session_t *session, responseHandler_t responseHandler,
		  uint32_t to_rate, uint32_t samples_per_second, uint32_t channels, char* lang, int interim, char *bugname, int single_utterance,
		  int separate_recognition, int max_alternatives, int profanity_filter, int word_time_offset, int punctuation, const char* model, int enhanced, 
		  const char* hints, char* play_file, void **ppUserData) {
      return google_speech_session_init<GStreamer_V2>(session, responseHandler, grpc_read_thread, to_rate, samples_per_second, channels,
        lang, interim, bugname, single_utterance, separate_recognition, max_alternatives, profanity_filter,
        word_time_offset, punctuation, model, enhanced, hints, play_file, ppUserData);
    }

}
