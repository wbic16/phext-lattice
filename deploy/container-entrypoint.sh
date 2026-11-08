#!/bin/sh
# container-entrypoint.sh — map the container's environment contract
# onto phext-edit's flags, then replace this shell with the server.
#
# The defaults mirror production (deploy/deploy.sh): the served
# directory, the home phext, and its coordinate. Sign-in configuration
# is NOT handled here — phext-edit reads the PHEXT_*, GITHUB_*,
# GOOGLE_*, and X_* variables from its own environment. The full
# contract is deploy/CONTAINER.md.
#
# Override wiring through the environment (PHEXT_EDIT_PORT etc.), not
# by appending flags: anything after the image name is appended to the
# phext-edit command line unchanged.
set -eu

data_dir="${PHEXT_DATA_DIR:-/data}"
home_file="${PHEXT_HOME:-choose-your-own-adventure.phext}"
home_coord="${PHEXT_HOME_COORD:-10.10.2/1.1.1/1.10.10}"
host="${PHEXT_EDIT_HOST:-0.0.0.0}"
port="${PHEXT_EDIT_PORT:-8080}"

exec phext-edit \
    --dir "$data_dir" \
    --home "$home_file" \
    --home-coord "$home_coord" \
    --host "$host" \
    --port "$port" \
    "$@"
