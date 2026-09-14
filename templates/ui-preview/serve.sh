#!/usr/bin/env bash
# Serve one checkout of this repository for the UI preview.
#
# Copy to scripts/ui-preview/serve.sh. Called with two variables:
#   TS_CHECKOUT  the directory to serve (the head, or the merge-base)
#   TS_PORT      the port to listen on
# Two copies run at once, so nothing here may assume a fixed port or a shared
# build directory. The action waits for the config's `ready` path to answer.
#
# This is the Next.js shape. A Vite app is `npm run build` then
# `npx vite preview --port "$TS_PORT" --strictPort`; anything else is whatever
# your `dev` script does, pointed at a mocked backend.
set -euo pipefail
cd "$TS_CHECKOUT"

npm ci --no-audit --no-fund
# Point the app at a mocked backend, never a real one: the preview must not
# read or write anything that exists.
export NEXT_PUBLIC_API_URL="http://127.0.0.1:${TS_PORT}/api"
npm run build
exec npx next start --port "$TS_PORT" --hostname 127.0.0.1
