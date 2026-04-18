#!/usr/bin/env bash
# Runs mint recovery test using V2 (slice-based) recovery

set -euo pipefail
export RUST_LOG="${RUST_LOG:-info}"

source scripts/_common.sh
build_workspace
add_target_dir_to_path
make_fm_test_marker

mint-module-tests recovery-v2
