#!/bin/sh
set -eu
export UV_CACHE_DIR="${UV_CACHE_DIR:-/tmp/uv-cache}"
# Controller judgment is supplied only outside the reader environment.
# Without it, native metrics are available and grounding remains unknown.
# Harbor restores an exported artifact at its source path in the separate
# verifier; destination controls the host collection path only.
exec uv run --project /tests --no-sync --offline python /tests/verify.py --answer /app/answer.json --rewardkit
