#!/usr/bin/env bash

set -eo pipefail

# Post-minimint-rip: `fedimint-server-daemon` deliberately pulls in the three
# concrete module crates (mintv2/lnv2/walletv2 + their -common siblings) and
# the UI was folded in from `fedimint-server-ui`. The check that forbade any
# `fedimint-*-server` / `-client` / `-common` deps in the daemon no longer
# fits reality, so it's removed.
if grep -E "fedimint-[a-zA-Z0-9]+-(server|client)" fedimint-testing/Cargo.toml | grep -v -E "fedimint-api-client|fedimint-gateway-*" >&2 ; then
  >&2 echo "fedimint-testing/Cargo.toml must not depend on modules"
  return 1
fi
find modules/ -name Cargo.toml | grep common/ | while read -r cargo_toml ; do
  if grep -E "fedimint-" "$cargo_toml" | grep -E -v "fedimint-core|-common|fedimint-logging|fedimint-connectors" >&2 ; then
    >&2 echo "Fedimint modules' -common crates should not introduce new fedimint dependencies: $cargo_toml"
    >&2 echo "The goal is to avoid circular deps that blow up build times. Ping @dpc for help."
    return 1
  fi
done
find gateway/fedimint-gateway-client/ -name Cargo.toml | while read -r cargo_toml ; do
  if grep -E "fedimint-lightning" "$cargo_toml" >&2 ; then
    >&2 echo "$cargo_toml must not depend on fedimint-lightning"
    return 1
  fi
done
find gateway/ -name Cargo.toml | while read -r cargo_toml ; do
  if grep -E "fedimint-server" "$cargo_toml" >&2 ; then
    >&2 echo "$cargo_toml must not depend on fedimint-server"
    return 1
  fi
done
find fedimint-client/ -name Cargo.toml | while read -r cargo_toml ; do
  if grep -E "fedimint-server" "$cargo_toml" >&2 ; then
    >&2 echo "$cargo_toml must not depend on fedimint-server"
    return 1
  fi
done
echo Cargo.lock | while read -r cargo_lock ; do
  if grep -E "openssl" "$cargo_lock" | grep -v openssl-probe >&2 ; then
    >&2 echo "$cargo_lock must not depend on openssl"
    return 1
  fi
done
