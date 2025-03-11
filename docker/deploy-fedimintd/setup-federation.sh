#!/usr/bin/env bash

set -euo pipefail

##
## Args handling
##

if [ "$#" -lt 4 ]; then
    echo "Usage: $0 <domain1:port1> <domain2:port2> <domain3:port3> <domain4:port4> [domain5:port5...]"
    echo "Example: $0 localhost:8271 localhost:8272 localhost:8273 localhost:8274"
    exit 1
fi

# Store all domains in an array
domains=("$@")
num_domains=${#domains[@]}

# Function to wait for server status
wait_for_status() {
    local host="$1"
    local expected_status="$2"
    local max_attempts=30
    local attempt=1

    while [ $attempt -le $max_attempts ]; do
        # Get the status and trim the quotes
        local raw_status=$(fedimint-cli --password "password" admin config-gen --ws "ws://$host" server-status)
        local status=$(echo $raw_status | tr -d '"')
        
        if [ "$status" = "$expected_status" ]; then
            return 0
        fi
        echo "Waiting for status $expected_status on $host (attempt $attempt/$max_attempts)..."
        sleep 2
        attempt=$((attempt + 1))
    done
    return 1
}

##
## Main setup process
##

# Step 0: Wait for all nodes to be in AwaitingLocalParams state
echo "Waiting for all nodes to be ready..."

for host in "${domains[@]}"; do
    echo "Waiting for $host to be ready..."
    if ! wait_for_status "$host" "AwaitingLocalParams"; then
        echo "Error: Node $host failed to reach AwaitingLocalParams state"
        exit 1
    fi
done

# Step 1: Set local parameters for each node and collect connection info
echo "Setting local parameters for each node..."

connection_infos=()
for i in "${!domains[@]}"; do
    host="${domains[$i]}"
    if [ $i -eq 0 ]; then
        # Leader node
        echo "Setting leader parameters for $host..."
        raw_connection_info=$(fedimint-cli --password "password" admin config-gen --ws "ws://$host" set-local-params "Devimint Guardian $i" --federation-name "Devimint Federation")
        # Remove the quotes that surround the connection info
        connection_info=$(echo $raw_connection_info | tr -d '"')
        connection_infos[$i]="$connection_info"
    else
        # Follower nodes
        echo "Setting follower parameters for $host..."
        raw_connection_info=$(fedimint-cli --password "password" admin config-gen --ws "ws://$host" set-local-params "Devimint Guardian $i")
        # Remove the quotes that surround the connection info
        connection_info=$(echo $raw_connection_info | tr -d '"')
        connection_infos[$i]="$connection_info"
    fi
done

echo "Successfully retrieved connection info for all ${#connection_infos[@]} nodes"

# Step 2: Exchange peer connection information
echo "Exchanging peer connection information..."

# Share connection info between nodes
for i in "${!domains[@]}"; do
    host="${domains[$i]}"
    for j in "${!domains[@]}"; do
        if [ $i -ne $j ]; then
            echo "Sharing connection info from node $j to node $i..."
            fedimint-cli --password "password" admin config-gen --ws "ws://$host" add-peer-connection-info "${connection_infos[$j]}"
        fi
    done
done

# Step 3: Start DKG on all nodes
echo "Starting DKG on all nodes..."

for host in "${domains[@]}"; do
    echo "Starting DKG on $host..."
    fedimint-cli --password "password" admin config-gen --ws "ws://$host" start-dkg
done

# Step 4: Wait for consensus to be running on all nodes
echo "Waiting for consensus to be running on all nodes..."

for host in "${domains[@]}"; do
    echo "Waiting for consensus on $host..."
    if ! wait_for_status "$host" "ConsensusRunning"; then
        echo "Error: Failed to reach consensus on $host"
        exit 1
    fi
done

echo "Federation setup completed successfully!"
