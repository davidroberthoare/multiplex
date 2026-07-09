#!/usr/bin/env bash
# Bundle cuemesh2 binaries with GStreamer dylibs into a portable .tar.gz
# for macOS. Requires dylibbundler (brew install dylibbundler).
set -euo pipefail

VERSION="${1:?usage: $0 <version-tag>}"
ARCHIVE_NAME="cuemesh2-${VERSION}-macos"
STAGING="dist/${ARCHIVE_NAME}"
LIBDIR="${STAGING}/lib"
PLUGDIR="${STAGING}/plugins"
BINDIR="${STAGING}"

rm -rf "$STAGING"
mkdir -p "$LIBDIR" "$PLUGDIR"

echo "==> Copying universal binaries ..."
cp target/release/cuemesh2-controller "$BINDIR/"
cp target/release/cuemesh2-client "$BINDIR/"

# Versions/1.0 is where the .pkg actually installs; the Versions/Current
# symlink is not reliably present on CI runners.
GST_FRAMEWORK="/Library/Frameworks/GStreamer.framework/Versions/1.0"
GST_PLUGIN_DIR="${GST_FRAMEWORK}/lib/gstreamer-1.0"

echo "==> Bundling GStreamer dylibs with dylibbundler ..."
# -od  = overwrite destination
# -b   = backup existing (not needed with -od)
# -x   = fix the given executable
# -d   = dylib destination directory
# -p   = path prefix to embed in the binary
# -s   = where to resolve @rpath/... references (gstreamer-rs links the
#        client against @rpath/libgst*.dylib); without it dylibbundler
#        prompts interactively for the path, which hangs forever in CI.
# echo quit | : belt-and-suspenders — if a prompt still appears, abort
#        with an error instead of hanging.
echo quit | dylibbundler -od -b -x "$BINDIR/cuemesh2-controller" \
  -d "$LIBDIR" -p @executable_path/lib/ -s "${GST_FRAMEWORK}/lib"
echo quit | dylibbundler -od -b -x "$BINDIR/cuemesh2-client" \
  -d "$LIBDIR" -p @executable_path/lib/ -s "${GST_FRAMEWORK}/lib"

echo "==> Copying GStreamer plugins ..."
if [ -d "$GST_PLUGIN_DIR" ]; then
  cp -a "$GST_PLUGIN_DIR"/. "$PLUGDIR/"
else
  echo "WARNING: could not find GStreamer plugin directory at ${GST_PLUGIN_DIR}. Plugins not bundled."
fi

echo "==> Creating launcher scripts ..."
cat > "$BINDIR/run-controller.sh" << 'SCRIPT'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
export DYLD_LIBRARY_PATH="$DIR/lib"
export GST_PLUGIN_PATH="$DIR/plugins"
exec "$DIR/cuemesh2-controller" "$@"
SCRIPT

cat > "$BINDIR/run-client.sh" << 'SCRIPT'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
export DYLD_LIBRARY_PATH="$DIR/lib"
export GST_PLUGIN_PATH="$DIR/plugins"
exec "$DIR/cuemesh2-client" "$@"
SCRIPT

chmod +x "$BINDIR/run-controller.sh" "$BINDIR/run-client.sh"

echo "==> Creating archive ..."
mkdir -p dist
tar czf "dist/${ARCHIVE_NAME}.tar.gz" -C "$(dirname "$STAGING")" "$(basename "$STAGING")"

echo "Done: dist/${ARCHIVE_NAME}.tar.gz"
