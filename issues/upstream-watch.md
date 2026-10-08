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
| kriklivsky | https://github.com/kriklivsky/freeswitch-modules | Fork of upstream. `main` is old jambonz upstream (to Oct 2024), nothing new. Only `fix/code-review-bugfixes` (5 commits) and `refactor/architecture-improvements` (9, includes the first 5) touch `mod_audio_fork`; both March 2026. |
| TopCS | https://github.com/TopCS/freeswitch-modules | Fork of upstream. Branches: `main`, `dev`, `codex/update-mod_dialogflow-to-latest-google-sdk`, `feature/whisper-spool`. |

## acclaim-ai: last checked 2026-10-08 (branch tips as of push 2026-09-30)

Not taken yet:

- [ ] `stop_play <uuid> <bugname>` (0f0624a, `vux-63-add-silerovad…`): one line, `if (argc > 2) bugname = argv[2];`. Our `mod_audio_fork.c` still always uses the default bug name.
- [ ] Lower streaming prebuffer, 160/80 ms to 80/20 ms (07143fc, `sip-179-ws2grpc-120ms-latency`): latency vs underrun trade-off, test with jitter first.
- [ ] `fix-locked-thread-leak`: check that our connect-failure and `switch_core_media_bug_add` failure paths free the pipe (they delete the `AudioPipe` on `CONNECTION_ERROR`, on synchronous `lws_client_connect_via_info` failure, and call `fork_session_destroy()` on `bug_add` failure). The rest is already covered by our `robustness-fixes` rework.

Deliberately skipped: raw OPUS over the WebSocket (255fb5f / 43105e1, changes the wire protocol), standalone Makefile (Docker build covers it), `mod_vad_silero`, `mod_aws_transcribe_ws`, leak-hunting log instrumentation, 2024 jambonz commits for other modules.

## kriklivsky: last checked 2026-10-08 (branch tips as of push 2026-03-23)

Most of it is already covered by our `robustness-fixes` work (null checks, cleanup locking, termination, exceptions, connect-failure cleanup). Not taken yet:

- [ ] Receive buffer as `std::vector` plus an overflow flag (`refactor/architecture-improvements`, 17e5fa5 / 8ea5344): discards the remaining fragments of an oversized message. Would close the two open Medium items on the first fragment allocation and the oversized tail in `review-findings.md`.
- [ ] `{"type":"hangup"}` message hangs up the channel with NORMAL_CLEARING and fires `mod_audio_fork::hangup` (`fix/code-review-bugfixes`, a65a94f). Only new feature; lets the remote end terminate calls, so decide first whether we want it.
- [ ] Reserve and free the missing event subclasses (`CONNECT_SUCCESS`, `CONNECT_FAIL`, `BUFFER_OVERRUN`, `JSON`): matches a Low item in `review-findings.md`.

Deliberately skipped: `std::vector` audio buffer (conflicts with our `shared_ptr` ownership), locking around `pAudioPipe = nullptr` (covered by `PipeHandle`), removal of the duplicate `else if` branches (dead code, already gone here).

## TopCS: last checked 2026-10-08 (branch tips as of push 2026-09-30)

Only one commit touches `mod_audio_fork` (e907b61 "build indepence", on `main`, `dev` and `feature/whisper-spool`). The rest is `mod_dialogflow` (CX API, events, packaging), a new CES module and Dialogflow backchannel work; ignore.

Deliberately skipped: standalone CMake build with `FindSpeexDSP.cmake` plus README section (Docker build covers it; the file list is also stale, no `vector_math`). No code changes to the module.

## Fork network sweep: checked 2026-10-08

Walked the whole fork network of lylepratt/freeswitch-modules with `gh api repos/<r>/forks` (48 direct forks, 8 second-level ones), compared every branch of every fork that was pushed after Feb 2024 against `kriklivsky:main` (plain jambonz upstream up to Oct 2024), and looked only at `mod_audio_fork/`. Nothing new beyond what is listed above:

- Most other forks only carry the jambonz upstream history (about 84 commits ahead of lylepratt, no own changes to `mod_audio_fork`). They are not worth watching: sptmru (+bjzhoum, rasheed-speechlogix), Catharsis68 (+Vicfou-dev), siwting, bitvizor, hnimminh, kietcaodev, bryananderson, baron2050, ochui, callable-ltd (+sip-rtc), rammohanyadavalli, goddva, vdharashive, cyrenity, ordinerf, murugancmi, areski, ahmedggalal. The 19 forks whose last push is the fork date (2024-02-04) were not compared; they have no own commits.
- lonelyxmas (+ringover), branch `fix/azure-tts-no-device-output`: wraps the `lws_glue.cpp` resampling in try/catch (June 2024); an old jambonz commit, our `robustness-fixes` rework covers it.
- RomanKok (parent of acclaim): same branches as acclaim (`fix_audio_fork`, `support_opus`), already covered in the acclaim section.
- WorkingTheoryAS is a fork of our own repo (questnet), pushed 2026-10-03; its branch `questnet` is ours.
- justinamalonson: branches could not be listed (404), probably empty or gone.
- To re-run the sweep, compare each fork's branches against `kriklivsky:main` or against our `questnet` and filter the file list for `mod_audio_fork`. Forks with `pushed_at` older than their `created_at` + a few minutes have no own commits.
