# freeswitch-modules

The `mod_audio_fork` FreeSWITCH module, intended for use with a [jambonz](https://jambonz.org) programmable voice platform deployment. It is written in Rust; the interface is described in [mod_audio_fork/README.md](mod_audio_fork/README.md).

| Crate | Role |
| --- | --- |
| `audio_fork` | The core: argument parsing, the socket task, capture and playout; no FreeSWITCH dependency |
| `freeswitch` | Bindings to the FreeSWITCH functions the module calls (generated at build time) and a thin safe layer |
| `mod_audio_fork` | The module itself (`cdylib`): FreeSWITCH callbacks, the runtime and the registry of forks |
| `test_server` | Scripted WebSocket server for the tests |
| `fs_tests` | Scenarios and the load test against a real FreeSWITCH |

## Development

Tasks are run with [just](https://github.com/casey/just). Building and testing need Docker; IDE setup needs `git`.

| Target | Description |
| --- | --- |
| `just` / `just test` | `cargo fmt --check`, `clippy`, unit and simulation tests, and the release build of `mod_audio_fork.so` |
| `just test-fs [filter]` | Scenarios against a real FreeSWITCH 1.11.3 over ESL, and the panic scenarios against the `test-hooks` build |
| `just test-load [n]` | `n` calls (default 20) with bidirectional audio for 60 s: CPU per call, buffer overruns, playout delay |
| `just prereqs` | Check that Docker is installed and running |
| `just rebuild` | Rebuild from scratch, ignoring the Docker cache |
| `just shell` | Interactive shell in the fs-test image |
| `just ide-setup` | Fetch the FreeSWITCH headers into `.deps/` and write a local `.cargo/config.toml` so rust-analyzer can build the bindings |
| `just clean` | Remove the Docker images created by the justfile |

## Licensing

This software is available under a dual-licensing scheme.  For specific use in a standalone [jambonz](https://jambonz.org) deployment, the [MIT License](./LICENSE_MIT) applies.  For all other uses, the software is licensed for use under the [AGPL Version 3.0 license](./LICENSE_AGPL-3.0).

Please review [COPYING](./COPYING.md) for detailed information in order to assess whether your intended usage meets the specific conditions that allow for usage under the MIT License.

## Contributing to the software

If you wish to contribute changes, please review [our rules](./CONTRIBUTING.md) governing contributions.