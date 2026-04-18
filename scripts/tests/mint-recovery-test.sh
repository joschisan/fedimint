#!/usr/bin/env bash
# Runs mint recovery test using V1 (session-based) recovery with reissuance

set -euo pipefail
export RUST_LOG="${RUST_LOG:-info}"
export FM_FORCE_V1_MINT_RECOVERY=1

source scripts/_common.sh
build_workspace
add_target_dir_to_path
make_fm_test_marker

mint-module-tests recovery-v1
