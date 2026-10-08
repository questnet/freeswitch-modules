# Test and build helpers for mod_audio_fork (compile-only check + unit tests, run in Docker)

image := "mod-audio-fork-test"
fs_version := "v1.11.3"
lws_version := "v5.0.0"

default: test

# Check that the required tooling is installed
prereqs:
    @command -v docker >/dev/null || { echo "docker is required: https://docs.docker.com/get-docker/"; exit 1; }
    @docker info >/dev/null 2>&1 || { echo "docker daemon is not running"; exit 1; }
    @echo "prerequisites ok"

# Compile all sources against FreeSWITCH/libwebsockets headers and run unit tests (native arch, scalar vector math)
test: prereqs
    docker build --tag {{image}} --build-arg FS_VERSION={{fs_version}} --build-arg LWS_VERSION={{lws_version}} .

# Same, on linux/amd64 with SSE2 vector math (emulated on arm hosts)
test-sse2: prereqs
    docker build --platform linux/amd64 --tag {{image}}-sse2 --build-arg SIMD=sse2 --build-arg FS_VERSION={{fs_version}} --build-arg LWS_VERSION={{lws_version}} .

# Run both configurations
test-all: test test-sse2

# Rebuild from scratch, ignoring the Docker cache
rebuild: prereqs
    docker build --no-cache --tag {{image}} .

# Try other versions, e.g. `just try-versions v1.11.3 v5.0.0`
try-versions fs lws: prereqs
    docker build --tag {{image}}-try --build-arg FS_VERSION={{fs}} --build-arg LWS_VERSION={{lws}} .

# Interactive shell in the built image for debugging
shell: test
    docker run --rm -it {{image}} bash

# Remove the images created by this file
clean:
    -docker rmi {{image}} {{image}}-sse2 {{image}}-try

# Fetch FreeSWITCH/libwebsockets headers into .deps and generate IDE config (see scripts/ide-setup.sh)
ide-setup:
    scripts/ide-setup.sh {{fs_version}} {{lws_version}}
