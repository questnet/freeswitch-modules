# Compile-only check of mod_audio_fork against FreeSWITCH headers and libwebsockets,
# plus standalone unit tests. Nothing is linked against libfreeswitch.
#   docker build .
#   docker build --platform linux/amd64 --build-arg SIMD=sse2 .
FROM debian:trixie-slim

ARG FS_VERSION=v1.11.3
ARG LWS_VERSION=v5.0.0
ARG SIMD=none

RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake pkg-config git ca-certificates \
      libboost-dev libspeexdsp-dev zlib1g-dev \
      libssl-dev libcurl4-openssl-dev libpcre2-dev libsqlite3-dev libedit-dev libldns-dev \
    && rm -rf /var/lib/apt/lists/*

# FreeSWITCH: headers only
RUN git clone --depth 1 --branch ${FS_VERSION} https://github.com/signalwire/freeswitch.git /src/freeswitch

# libwebsockets: minimal build, no TLS
RUN git clone --depth 1 --branch ${LWS_VERSION} https://github.com/warmcat/libwebsockets.git /src/lws \
    && cmake -S /src/lws -B /src/lws/build \
         -DCMAKE_BUILD_TYPE=Release -DLWS_WITH_SSL=OFF \
         -DLWS_WITHOUT_TESTAPPS=ON -DLWS_WITHOUT_TEST_SERVER=ON \
         -DLWS_WITHOUT_TEST_SERVER_EXTPOLL=ON -DLWS_WITHOUT_TEST_PING=ON \
         -DLWS_WITHOUT_TEST_CLIENT=ON -DLWS_WITH_MINIMAL_EXAMPLES=OFF \
    && cmake --build /src/lws/build -j"$(nproc)" \
    && cmake --install /src/lws/build

# Generate the build-time headers without running configure (LP64 values)
RUN cd /src/freeswitch/src/include \
    && sed -e 's/@short_value@/short/' -e 's/@int_value@/int/' -e 's/@long_value@/long/' \
           -e 's/@size_t_value@/unsigned long/' -e 's/@ssize_t_value@/long/' \
           -e 's/@voidp_size@/8/' -e 's|@prefix@|/usr/local/freeswitch|' \
           -e 's/^@[a-z0-9_]*_fmt@$//' switch_am_config.h.in > switch_am_config.h \
    && sed -E 's/@([A-Z_]+)@/0/g' switch_version.h.template > switch_version.h

COPY mod_audio_fork /work/mod_audio_fork
WORKDIR /work/mod_audio_fork

ENV FS_INC="-I/src/freeswitch/src/include -I/src/freeswitch/libs/libteletone/src"

# Compile gate (-c only, no link)
RUN set -eux; \
    case "$SIMD" in sse2) SIMDFLAGS="-msse2 -DUSE_SSE2";; avx2) SIMDFLAGS="-mavx2 -DUSE_AVX2";; *) SIMDFLAGS="";; esac; \
    echo "$SIMDFLAGS" > /tmp/simdflags; \
    mkdir -p /tmp/obj; \
    gcc -c -Wall $FS_INC $(pkg-config --cflags libwebsockets) mod_audio_fork.c -o /tmp/obj/mod_audio_fork.o; \
    for f in lws_glue parser audio_pipe vector_math; do \
      g++ -c -Wall -std=c++11 $SIMDFLAGS $FS_INC $(pkg-config --cflags libwebsockets) $f.cpp -o /tmp/obj/$f.o; \
    done

# Unit tests (link only vector_math + base64, no FreeSWITCH)
RUN set -eux; \
    g++ -Wall -std=c++11 $(cat /tmp/simdflags) -I. test/test_units.cpp vector_math.cpp -o /tmp/test_units; \
    /tmp/test_units
