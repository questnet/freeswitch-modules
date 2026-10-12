# Test image for mod_audio_fork, see docs/testing-plan.md#image.
#   just test      (stage rust: fmt, clippy, unit and simulation tests, release build)
#   just test-fs   (stage fs-test: real FreeSWITCH driven over ESL)

# FreeSWITCH with only the modules the tests need
FROM debian:trixie-slim AS fs-build

ARG FS_VERSION=v1.11.3
ARG SOFIA_VERSION=v1.13.18
ARG SPANDSP_VERSION=v3.1.1

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential autoconf automake libtool libtool-bin pkg-config git ca-certificates cmake \
      zlib1g-dev libssl-dev libcurl4-openssl-dev libpcre2-dev libsqlite3-dev libedit-dev libldns-dev \
      libspeex-dev libspeexdsp-dev libsndfile1-dev libtiff-dev libjpeg-dev uuid-dev \
    && rm -rf /var/lib/apt/lists/*

# FreeSWITCH needs newer spandsp and sofia-sip than Debian ships
RUN git clone --depth 1 --branch ${SPANDSP_VERSION} https://github.com/freeswitch/spandsp.git /src/spandsp \
    && cd /src/spandsp \
    && ./bootstrap.sh && ./configure --prefix=/usr/local && make -j"$(nproc)" && make install
RUN git clone --depth 1 --branch ${SOFIA_VERSION} https://github.com/freeswitch/sofia-sip.git /src/sofia-sip \
    && cd /src/sofia-sip && ./bootstrap.sh && ./configure --prefix=/usr/local && make -j"$(nproc)" && make install \
    && ldconfig

RUN git clone --depth 1 --branch ${FS_VERSION} https://github.com/signalwire/freeswitch.git /src/freeswitch
COPY tests/modules.conf /src/freeswitch/modules.conf
RUN cd /src/freeswitch && ./bootstrap.sh -j \
    && ./configure --prefix=/usr/local/freeswitch --disable-libvpx --disable-libyuv --without-pgsql \
    && make -j"$(nproc)" && make install \
    && ldconfig
RUN echo /usr/local/freeswitch/lib > /etc/ld.so.conf.d/freeswitch.conf && ldconfig
ENV PATH=/usr/local/freeswitch/bin:$PATH

# The C module, for the differential tests (T4) until its sources are deleted
FROM fs-build AS c-module
ARG LWS_VERSION=v5.0.0
RUN apt-get update && apt-get install -y --no-install-recommends libboost-dev && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch ${LWS_VERSION} https://github.com/warmcat/libwebsockets.git /src/lws \
    && cmake -S /src/lws -B /src/lws/build \
         -DCMAKE_BUILD_TYPE=Release -DLWS_WITH_SSL=OFF \
         -DLWS_WITHOUT_TESTAPPS=ON -DLWS_WITHOUT_TEST_SERVER=ON \
         -DLWS_WITHOUT_TEST_SERVER_EXTPOLL=ON -DLWS_WITHOUT_TEST_PING=ON \
         -DLWS_WITHOUT_TEST_CLIENT=ON -DLWS_WITH_MINIMAL_EXAMPLES=OFF \
    && cmake --build /src/lws/build -j"$(nproc)" \
    && cmake --install /src/lws/build && ldconfig
COPY mod_audio_fork/*.c mod_audio_fork/*.h mod_audio_fork/*.cpp mod_audio_fork/*.hpp /work/mod_audio_fork/
COPY mod_audio_fork/test /work/mod_audio_fork/test
WORKDIR /work/mod_audio_fork
RUN set -eux; \
    inc="-I/usr/local/freeswitch/include/freeswitch"; \
    mkdir -p /tmp/obj /out/c; \
    gcc -c -fPIC -Wall $inc mod_audio_fork.c -o /tmp/obj/mod_audio_fork.o; \
    for f in lws_glue parser audio_pipe vector_math; do \
      g++ -c -fPIC -Wall -std=c++17 $inc $f.cpp -o /tmp/obj/$f.o; \
    done; \
    g++ -shared -o /out/c/mod_audio_fork.so /tmp/obj/*.o \
      -L/usr/local/freeswitch/lib -lfreeswitch -lwebsockets -lspeexdsp; \
    g++ -Wall -std=c++17 -I. test/test_units.cpp vector_math.cpp -o /tmp/test_units && /tmp/test_units

# fmt, clippy, unit and simulation tests, release builds (with and without test-hooks)
FROM fs-build AS rust
RUN apt-get update && apt-get install -y --no-install-recommends curl clang libclang-dev && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
WORKDIR /work
COPY rust-toolchain.toml .
RUN rustup show active-toolchain || rustup toolchain install
ENV FREESWITCH_INCLUDE_DIR=/usr/local/freeswitch/include/freeswitch FREESWITCH_LIB_DIR=/usr/local/freeswitch/lib
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/work/target set -eux; \
    cargo fmt --all --check; \
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings; \
    cargo test --locked; \
    cargo test --locked -p audio_fork --features test-hooks --test panics; \
    mkdir -p /out/rust /out/rust-hooks; \
    cargo build --release --locked -p mod_audio_fork; \
    cp target/release/libmod_audio_fork.so /out/rust/mod_audio_fork.so; \
    cargo build --release --locked -p mod_audio_fork --features test-hooks; \
    cp target/release/libmod_audio_fork.so /out/rust-hooks/mod_audio_fork.so; \
    nm -D --defined-only /out/rust/mod_audio_fork.so | grep -q ' mod_audio_fork_module_interface$'; \
    for t in fs load; do \
      exe="$(cargo test --release --locked -p fs_tests --test $t --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))$/\1/p')"; \
      cp "$exe" /out/fs_tests_$t; \
    done

# FreeSWITCH, both module builds and the fs_tests binary
FROM fs-build AS fs-test
COPY --from=c-module /usr/local/lib/libwebsockets.so* /usr/local/lib/
RUN ldconfig
COPY --from=c-module /out/c /opt/mod_audio_fork/c
COPY --from=rust /out/rust /opt/mod_audio_fork/rust
COPY --from=rust /out/rust-hooks /opt/mod_audio_fork/rust-hooks
COPY --from=rust /out/fs_tests_fs /usr/local/bin/fs_tests
COPY --from=rust /out/fs_tests_load /usr/local/bin/fs_load
COPY tests/fs-conf /opt/fs-conf
ENV FREESWITCH_BIN=/usr/local/freeswitch/bin/freeswitch \
    FREESWITCH_MOD_DIR=/usr/local/freeswitch/lib/freeswitch/mod \
    AUDIO_FORK_MODULE_DIR=/opt/mod_audio_fork \
    FS_TEST_CONF=/opt/fs-conf
CMD ["fs_tests"]
