#!/bin/bash
set -e

# Verify that DEPENDENCY_CONFIG exists and contains bitcoind configuration
if [ -z "$DEPENDENCY_CONFIG" ]; then
    echo "Error: DEPENDENCY_CONFIG environment variable is not set"
    echo "Bitcoin Core must be installed and running for Fedimint to function"
    exit 1
fi

# Install jq if needed
if ! command -v jq &> /dev/null; then
    apt-get update && apt-get install -y jq
fi

# Verify bitcoind configuration exists in DEPENDENCY_CONFIG
if ! echo "$DEPENDENCY_CONFIG" | jq -e '.bitcoind' > /dev/null; then
    echo "Error: Bitcoin Core configuration not found in DEPENDENCY_CONFIG"
    echo "Bitcoin Core must be installed and running for Fedimint to function"
    exit 1
fi

# Extract values using jq
BITCOIN_USERNAME=$(echo "$DEPENDENCY_CONFIG" | jq -r '.bitcoind.rpc.username')
BITCOIN_PASSWORD=$(echo "$DEPENDENCY_CONFIG" | jq -r '.bitcoind.rpc.password')
BITCOIN_HOST=$(echo "$DEPENDENCY_CONFIG" | jq -r '.bitcoind.internal_address')
BITCOIN_PORT=$(echo "$DEPENDENCY_CONFIG" | jq -r '.bitcoind.rpc.port')

# Set Fedimintd environment variables
export FM_BITCOIN_RPC_KIND="bitcoind"
export FM_BITCOIN_RPC_URL="http://${BITCOIN_USERNAME}:${BITCOIN_PASSWORD}@${BITCOIN_HOST}:${BITCOIN_PORT}"
echo "Configured Bitcoin RPC URL from Start9 dependency"

# Set data directory
export FM_DATA_DIR="/data"

# Set public address from TOR_ADDRESS if available
if [ -n "$TOR_ADDRESS" ]; then
    export FM_P2P_URL="fedimint://${TOR_ADDRESS}:8173"
    export FM_API_URL="wss://${TOR_ADDRESS}/ws"
    echo "Using Tor address: $TOR_ADDRESS"
else
    export FM_P2P_URL="fedimint://localhost:8173"
    export FM_API_URL="ws://localhost:8174"
    echo "No Tor address found, using localhost"
fi

# Set binding addresses
export FM_BIND_P2P="0.0.0.0:8173"
export FM_BIND_API="0.0.0.0:8174"
export FM_BIND_UI="0.0.0.0:8175"
export FM_BITCOIN_NETWORK="bitcoin"

# Force Iroh networking
export FM_FORCE_IROH="true"
echo "Forcing Iroh networking for P2P connections"

# Print configuration for debugging
echo "Fedimintd configuration:"
echo "FM_DATA_DIR: $FM_DATA_DIR"
echo "FM_BITCOIN_RPC_KIND: $FM_BITCOIN_RPC_KIND"
echo "FM_BITCOIN_RPC_URL: $FM_BITCOIN_RPC_URL (sensitive)"
echo "FM_P2P_URL: $FM_P2P_URL"
echo "FM_API_URL: $FM_API_URL"

# Start Fedimintd
echo "Starting Fedimintd..."
exec fedimintd --data-dir "$FM_DATA_DIR" 