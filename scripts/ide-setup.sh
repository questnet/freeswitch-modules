#!/usr/bin/env bash
# Fetch FreeSWITCH/libwebsockets headers into .deps and write .clangd and
# .vscode/c_cpp_properties.json so the IDE can resolve includes.
# Usage: scripts/ide-setup.sh <fs_version> <lws_version>
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: $0 <fs_version> <lws_version>" >&2; exit 1; }
fs_version="$1"
lws_version="$2"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
command -v cmake >/dev/null || { echo "cmake is required"; exit 1; }
deps="$root/.deps"
mkdir -p "$deps"
[ -d "$deps/freeswitch" ] || git clone --depth 1 --branch $fs_version https://github.com/signalwire/freeswitch.git "$deps/freeswitch"
[ -d "$deps/lws" ] || git clone --depth 1 --branch $lws_version https://github.com/warmcat/libwebsockets.git "$deps/lws"
# Build-time headers FreeSWITCH normally generates via configure (LP64 values, as in the Dockerfile)
inc="$deps/freeswitch/src/include"
sed -e 's/@short_value@/short/' -e 's/@int_value@/int/' -e 's/@long_value@/long/' \
    -e 's/@size_t_value@/unsigned long/' -e 's/@ssize_t_value@/long/' \
    -e 's/@voidp_size@/8/' -e 's|@prefix@|/usr/local/freeswitch|' \
    -e 's/^@[a-z0-9_]*_fmt@$//' "$inc/switch_am_config.h.in" > "$inc/switch_am_config.h"
sed -E 's/@([A-Z_]+)@/0/g' "$inc/switch_version.h.template" > "$inc/switch_version.h"
# Configure only: generates lws_config.h, no compile needed
cmake -S "$deps/lws" -B "$deps/lws/build" -DLWS_WITH_SSL=OFF -DLWS_WITHOUT_TESTAPPS=ON \
    -DLWS_WITH_MINIMAL_EXAMPLES=OFF -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY >/dev/null
# Third-party headers from Homebrew (boost, speexdsp)
extra=""
if command -v brew >/dev/null; then
    for f in boost speexdsp; do
        p="$(brew --prefix "$f" 2>/dev/null || true)"
        if [ -d "$p/include" ]; then extra="$extra, -I$p/include"; else echo "missing: brew install $f"; fi
    done
fi
# clangd may pair an Xcode toolchain with the CLT SDK and then miss libc++; point it at the SDK's copy
cxx=""
if command -v xcrun >/dev/null; then cxx=", -isystem$(xcrun --show-sdk-path)/usr/include/c++/v1"; fi
cat > .clangd <<EOF
CompileFlags:
  Add: [-I$inc, -I$deps/freeswitch/libs/libteletone/src, -I$deps/lws/include, -I$deps/lws/build, -I$root/mod_audio_fork$extra]
---
If:
  PathMatch: .*\.(cpp|hpp|h)
CompileFlags:
  Add: [-xc++, -std=c++17$cxx]
EOF
# Same paths for the Microsoft C/C++ extension, which does not read .clangd
mkdir -p .vscode
incs="\"$inc\", \"$deps/freeswitch/libs/libteletone/src\", \"$deps/lws/include\", \"$deps/lws/build\", \"\${workspaceFolder}/mod_audio_fork\""
for f in boost speexdsp; do
    p="$(brew --prefix "$f" 2>/dev/null || true)"
    [ -d "$p/include" ] && incs="$incs, \"$p/include\""
done
cat > .vscode/c_cpp_properties.json <<EOF
{
  "version": 4,
  "configurations": [{
    "name": "mod_audio_fork",
    "includePath": [$incs],
    "cStandard": "c11",
    "cppStandard": "c++17"
  }]
}
EOF
echo "done: restart clangd / reload the IDE window"
