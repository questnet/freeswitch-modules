# freeswitch-modules

The `mod_audio_fork` Freeswitch module, intended for use with a [jambonz](https://jambonz.org) programmable voice platform deployment.

## Development

Tasks are run with [just](https://github.com/casey/just). Building and testing need Docker; IDE setup needs `git`.

| Target | Description |
| --- | --- |
| `just` / `just test` | `cargo fmt --check`, `clippy`, unit and simulation tests, and the release build of `mod_audio_fork.so` |
| `just test-fs [filter]` | Scenarios against a real FreeSWITCH 1.11.3 over ESL, once with the Rust and once with the C module |
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