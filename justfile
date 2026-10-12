# Test and build helpers for mod_audio_fork, all run in Docker (see docs/testing-plan.md)

image := "mod-audio-fork"
fs_version := "v1.11.3"
lws_version := "v5.0.0"
# extra `docker build` flags, e.g. the GitHub Actions cache in CI
build_flags := env("DOCKER_BUILD_FLAGS", "")

default: test

# Check that the required tooling is installed
prereqs:
    @command -v docker >/dev/null || { echo "docker is required: https://docs.docker.com/get-docker/"; exit 1; }
    @docker info >/dev/null 2>&1 || { echo "docker daemon is not running"; exit 1; }
    @echo "prerequisites ok"

# fmt, clippy, unit and simulation tests, and the release build of mod_audio_fork.so
test: prereqs
    docker build {{build_flags}} --target rust --tag {{image}}-rust --build-arg FS_VERSION={{fs_version}} .

# Real-FreeSWITCH scenarios against the Rust and the C module, and the panic scenarios against the
# test-hooks build; `just test-fs <filter>` runs a subset
test-fs filter="": prereqs
    docker build {{build_flags}} --target fs-test --tag {{image}}-fs-test --build-arg FS_VERSION={{fs_version}} --build-arg LWS_VERSION={{lws_version}} .
    docker run --rm -e AUDIO_FORK_MODULE=rust {{image}}-fs-test fs_tests {{filter}}
    docker run --rm -e AUDIO_FORK_MODULE=c {{image}}-fs-test fs_tests {{filter}}
    docker run --rm -e AUDIO_FORK_MODULE=rust-hooks {{image}}-fs-test fs_tests {{filter}}

# Rebuild from scratch, ignoring the Docker cache
rebuild: prereqs
    docker build --no-cache --target fs-test --tag {{image}}-fs-test .

# Interactive shell in the fs-test image for debugging
shell:
    docker run --rm -it {{image}}-fs-test bash

# Remove the images created by this file
clean:
    -docker rmi {{image}}-rust {{image}}-fs-test

# Fetch the FreeSWITCH headers into .deps for rust-analyzer (see scripts/ide-setup.sh)
ide-setup:
    scripts/ide-setup.sh {{fs_version}} {{lws_version}}
