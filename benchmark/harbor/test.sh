#!/bin/sh
set -eu
# Controller judgment is supplied only outside the reader environment.
# Without it, native metrics are available and grounding remains unknown.
exec uv run --project /tests --no-sync --offline python /tests/verify.py --rewardkit
