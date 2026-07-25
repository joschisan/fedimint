#!/usr/bin/env bash

set -euo pipefail
export RUST_LOG="${RUST_LOG:-info}"

source scripts/_common.sh
build_workspace
add_target_dir_to_path

# Disable LNv1 module if not in backwards compat test, or if gateway version >= 0.10.0
# Old gateways (< 0.10.0) require LNv1 module to be present
if [[ -z "${FM_BACKWARDS_COMPATIBILITY_TEST:-}" ]]; then
    export FM_ENABLE_MODULE_LNV1=0
elif [[ "${FM_GATEWAYD_BASE_IMAGE_VERSION:-}" == "v0.10"* ]] || \
     [[ "${FM_GATEWAYD_BASE_IMAGE_VERSION:-}" > "v0.10" ]]; then
    export FM_ENABLE_MODULE_LNV1=0
fi

# gatewaydv2 only supports the v2 mint and wallet modules, so run the test
# federation with v2 modules instead of v1.
export FM_ENABLE_MODULE_MINTV2=true
export FM_ENABLE_MODULE_MINT=false
export FM_ENABLE_MODULE_WALLETV2=true
export FM_ENABLE_MODULE_WALLET=false

gatewayv2-module-tests "$@"
