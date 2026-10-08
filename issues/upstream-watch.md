# Upstream watch

Repositories to check from time to time for features and fixes that may affect our `mod_audio_fork`. We only carry `mod_audio_fork`; ignore changes to other modules.

Remotes in this clone: `origin` (questnet), `upstream` (lylepratt). The others are not configured by default:
`git remote add acclaim https://github.com/acclaim-ai/freeswitch-modules.git`

How to check: `git fetch <remote>`, then for every branch (there are several, not just main)
`git log --no-merges --format='%h %ad %an %s' --date=short questnet..<remote>/<branch> -- mod_audio_fork`
and read the diffs of anything new. Update the "last checked" line below.

## Repositories

| Remote | URL | Notes |
|---|---|---|
| upstream | https://github.com/lylepratt/freeswitch-modules | Where our code came from. |
| acclaim | https://github.com/acclaim-ai/freeswitch-modules | Fork of RomanKok/freeswitch-modules (itself a fork). Branches: `main`, `fix-locked-thread-leak`, `fix_audio_fork`, `pre-opus`, `sip-179-ws2grpc-120ms-latency`, `standalone-makefile`, `support_opus`, `vux-63-add-silerovad-to-the-freeswitch`. |

## acclaim-ai: last checked 2026-10-08 (branch tips as of push 2026-09-30)

Not taken yet:

- [ ] `stop_play <uuid> <bugname>` (0f0624a, `vux-63-add-silerovad…`): one line, `if (argc > 2) bugname = argv[2];`. Our `mod_audio_fork.c` still always uses the default bug name.
- [ ] Lower streaming prebuffer, 160/80 ms to 80/20 ms (07143fc, `sip-179-ws2grpc-120ms-latency`): latency vs underrun trade-off, test with jitter first.
- [ ] `fix-locked-thread-leak`: check that our connect-failure and `switch_core_media_bug_add` failure paths free the pipe (they delete the `AudioPipe` on `CONNECTION_ERROR`, on synchronous `lws_client_connect_via_info` failure, and call `fork_session_destroy()` on `bug_add` failure). The rest is already covered by our `robustness-fixes` rework.

Deliberately skipped: raw OPUS over the WebSocket (255fb5f / 43105e1, changes the wire protocol), standalone Makefile (Docker build covers it), `mod_vad_silero`, `mod_aws_transcribe_ws`, leak-hunting log instrumentation, 2024 jambonz commits for other modules.
