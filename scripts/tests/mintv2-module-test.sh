#!/usr/bin/env bash

set -euo pipefail
export RUST_LOG="${RUST_LOG:-info}"

source scripts/_common.sh
build_workspace
add_target_dir_to_path

# Disable MintV1 module when running MintV2 tests
export FM_ENABLE_MODULE_MINTV2=1

mintv2-module-tests "$@"
