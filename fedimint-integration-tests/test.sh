#!/usr/bin/env bash
set -euo pipefail

CONTAINER_NAME="fm-integration-bitcoind"

cleanup() {
    echo "Cleaning up..."
    docker stop "$CONTAINER_NAME" 2>/dev/null || true
    docker rm "$CONTAINER_NAME" 2>/dev/null || true
}

trap cleanup EXIT

echo "Starting bitcoind in Docker..."
docker run -d \
    --name "$CONTAINER_NAME" \
    -p 18443:18443 \
    ruimarinho/bitcoin-core:latest \
    -regtest=1 \
    -rpcuser=bitcoin \
    -rpcpassword=bitcoin \
    -rpcallowip=0.0.0.0/0 \
    -rpcbind=0.0.0.0 \
    -rpcport=18443 \
    -fallbackfee=0.0004 \
    -txindex=0

echo "Waiting for bitcoind to start..."
sleep 3

echo "Creating wallet..."
docker exec "$CONTAINER_NAME" bitcoin-cli \
    -regtest -rpcuser=bitcoin -rpcpassword=bitcoin \
    createwallet "" || true

echo "Building workspace..."
cargo build -p fedimintd -p fedimint-cli -p fedimint-gateway-server -p fedimint-gateway-client -p fedimint-integration-tests

echo "Running integration tests..."
RUST_LOG="${RUST_LOG:-info}" cargo run -p fedimint-integration-tests

echo "Tests completed successfully!"
