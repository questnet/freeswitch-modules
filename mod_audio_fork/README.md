# mod_audio_fork

A FreeSWITCH module that attaches a media bug to a channel and streams its audio as L16 over a WebSocket to a server, and can play audio the server sends back to the caller. Written in Rust; see [docs/rust-rebuild-plan.md](../docs/rust-rebuild-plan.md) for the design and [docs/testing-plan.md](../docs/testing-plan.md) for the tests.

## API: `uuid_audio_fork`

Arguments are split like every FreeSWITCH API command: on spaces, `'...'` groups, `\` escapes, and at most 10 tokens, the last one taking the rest of the line. JSON containing spaces therefore has to be quoted.

Replies: `+OK Success`, `-ERR Operation Failed`, or `-USAGE: ...` for malformed arguments. The default bugname is `audio_fork`.

### start

```
uuid_audio_fork <uuid> start <url> <mix> <rate> [bugname] [metadata] [bidi] [bidi-stream] [bidi-rate]
```

- `url`: `ws://` or `wss://`. The connection asks for the subprotocol `audio.drachtio.org`.
- `mix`: `mono` (the caller), `mixed` (caller and what the caller hears, mixed), `stereo` (caller in channel 0, what the caller hears in channel 1).
- `rate`: `8k`, `16k`, or an integer that is a multiple of 8000, at most 64000. Audio is resampled from the codec rate if they differ.
- With 6 tokens, the sixth is the metadata if it starts with `{` or `[`, otherwise the bugname. With 7 to 9 tokens, they are the bugname and the metadata.
- With all 10 tokens, the bugname and metadata are ignored when empty (`''`), and:
  - `bidi`: `true` plays server audio to the caller, `playback` does too but sends no call audio (only with `mono`, other mixes are rejected), anything else neither.
  - `bidi-stream`: must be `true` for playout.
  - `bidi-rate`: the rate the server sends audio at, 0 to 64000; playout needs it to be non-zero.
- `metadata`: sent as the first text frame once connected, at most 1 MB.

`start` pre-answers the channel. It fails if that fails, if the channel has no read codec, if a fork with that bugname is already attached, or if the metadata is over 1 MB.

### stop

```
uuid_audio_fork <uuid> stop [bugname] [text]
```

With one argument, it is the text if it starts with `{` or `[`, otherwise the bugname. The text is sent as the last frame before the connection is closed (only if the connection is open). A fork stopped while it is still connecting sends nothing, not even the metadata. Fails if there is no such fork.

### send_text

```
uuid_audio_fork <uuid> send_text [bugname] <text>
```

Same disambiguation as `stop`. Each call is one text frame of at most 1 MB. Texts sent before the connection is open are queued (at most 1024 texts and 4 MB) and sent right after the metadata. Over a limit, or once the connection has failed or ended, the call fails; text is never truncated.

## WebSocket protocol

Sent to the server:

1. the metadata, if any, as a text frame;
2. the queued texts, in order;
3. the audio as binary frames: L16, little-endian, at the requested rate, interleaved for stereo.

Up to 2 s of audio is buffered while the server is slow; beyond that, frames are dropped and `buffer_overrun` fires once. Audio is only captured while the connection is open.

Received from the server, as text frames with a JSON `type` (a message without `type` counts as `json`):

| `type` | Effect |
|---|---|
| `killAudio` | fires `kill_audio`, interrupts what the channel is playing (`CF_BREAK`), discards the audio still waiting to be played |
| `disconnect` | fires `disconnect` with `data` as body; the connection and the call stay up |
| `error` | fires `error` with `data` as body |
| `json` | fires `json` with the whole message as body |
| anything else | ignored (logged at INFO) |

A text message over 1 MB closes the connection (`disconnect` fires).

Binary frames from the server are played to the caller when playout is on, and ignored otherwise. They are joined across frames (a frame may end mid-sample), resampled from `bidi-rate` to the codec rate, and mixed into what the caller hears with saturating addition. Playout starts once 160 ms of audio has arrived and continues in steps of 80 ms (`320 * factor * 4` and `* 2` samples at the server's rate, `factor = bidi-rate / codec rate`), so the last part of a stream, under one step, waits for more audio.

## Events

All are `CUSTOM` events, carry the channel's data and an `Audio-Fork-Bugname` header, and are reserved on load and freed on unload.

| Subclass | When | Body |
|---|---|---|
| `mod_audio_fork::connect` | the connection is open | none |
| `mod_audio_fork::connect_failed` | the connection could not be opened (also after 10 s) | `{"reason": "..."}` |
| `mod_audio_fork::disconnect` | the server closed or the connection broke; or the server's `disconnect` | none; the server's `data` |
| `mod_audio_fork::error` | the server's `error`; or a panic ended this fork | the server's `data`; `{"reason": "panic: ..."}` |
| `mod_audio_fork::json` | the server's `json` | the whole message |
| `mod_audio_fork::kill_audio` | the server's `killAudio` | none |
| `mod_audio_fork::buffer_overrun` | capture audio was dropped, once per fork | none |

A fork queues no event once it was stopped or its call hung up; one queued just before can still arrive after the `stop` reply, as events are delivered asynchronously anyway.

## Failure behaviour

A panic in the module ends only the fork it happened in: it is logged at CRIT, `error` fires and the bug is removed; other calls are not affected. Unloading the module removes every fork's bug and closes its connection; the calls stay up.

## Building and testing

See the [top-level README](../README.md). The module is the `mod_audio_fork` crate's `cdylib`, installed as `mod_audio_fork.so`; it is built against the FreeSWITCH headers named by `FREESWITCH_INCLUDE_DIR` and links `libfreeswitch` from `FREESWITCH_LIB_DIR`.
