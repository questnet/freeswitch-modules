# freeswitch-modules

The `mod_audio_fork` Freeswitch module, intended for use with a [jambonz](https://jambonz.org) programmable voice platform deployment.

## Development

Tasks are run with [just](https://github.com/casey/just). Building and testing need Docker; IDE setup needs `git`, `cmake`, and (on macOS) Homebrew.

| Target | Description |
| --- | --- |
| `just` / `just test` | Compile all sources against FreeSWITCH/libwebsockets headers and run the unit tests (native arch, scalar vector math) |
| `just test-sse2` | Same, on linux/amd64 with SSE2 vector math (emulated on arm hosts) |
| `just test-all` | Run both configurations |
| `just prereqs` | Check that Docker is installed and running |
| `just rebuild` | Rebuild from scratch, ignoring the Docker cache |
| `just try-versions <fs> <lws>` | Build against other versions, e.g. `just try-versions v1.11.3 v5.0.0` |
| `just shell` | Interactive shell in the built image |
| `just ide-setup` | Fetch FreeSWITCH/libwebsockets headers into `.deps/` and generate `.clangd` and `.vscode/c_cpp_properties.json` for IDE support. Needs `brew install boost speexdsp` on macOS |
| `just clean` | Remove the Docker images created by the justfile |

## Licensing

This software is available under a dual-licensing scheme.  For specific use in a standalone [jambonz](https://jambonz.org) deployment, the [MIT License](./LICENSE_MIT) applies.  For all other uses, the software is licensed for use under the [AGPL Version 3.0 license](./LICENSE_AGPL-3.0).

Please review [COPYING](./COPYING.md) for detailed information in order to assess whether your intended usage meets the specific conditions that allow for usage under the MIT License.

## Contributing to the software

If you wish to contribute changes, please review [our rules](./CONTRIBUTING.md) governing contributions.