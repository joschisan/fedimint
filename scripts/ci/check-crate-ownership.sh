#!/usr/bin/env bash

set -euo pipefail

# Check that all published workspace crates on crates.io are owned by
# exactly the required set of owners (currently dpc and elsirion).
#
# Usage: ./scripts/ci/check-crate-ownership.sh

REQUIRED_OWNERS=("dpc" "elsirion")

# Get all workspace crate names, excluding those with publish=false
get_published_crate_names() {
    cargo metadata --no-deps --format-version 1 \
        | jq -r '.packages[]
            | select(.source == null)
            | select(.publish == null or (.publish | length) > 0)
            | .name' \
        | sort
}

# Query crates.io for owners of a crate
get_crate_owners() {
    local crate="$1"
    local response
    response=$(curl -sS --retry 3 --retry-delay 2 \
        -H "User-Agent: fedimint-ci-ownership-check" \
        "https://crates.io/api/v1/crates/${crate}/owners")

    # Check if the crate exists on crates.io
    if echo "$response" | jq -e '.errors // empty | length > 0' > /dev/null 2>&1; then
        echo "NOT_PUBLISHED"
        return
    fi

    echo "$response" | jq -r '.users[]?.login' | sort
}

main() {
    local crates
    crates=$(get_published_crate_names)
    local crate_count
    crate_count=$(echo "$crates" | grep -c . || true)
    echo "Found ${crate_count} publishable workspace crates"

    local missing_owners=()
    local extra_owners=()
    local not_published=()
    local errors=0

    for crate in $crates; do
        echo -n "Checking ${crate}... "
        local owners
        owners=$(get_crate_owners "$crate")

        if [ "$owners" = "NOT_PUBLISHED" ]; then
            echo "not yet published on crates.io"
            not_published+=("$crate")
            # Rate limit: crates.io asks for max 1 req/sec
            sleep 1
            continue
        fi

        # Check for missing required owners
        local crate_missing=()
        for required in "${REQUIRED_OWNERS[@]}"; do
            if ! echo "$owners" | grep -qx "$required"; then
                crate_missing+=("$required")
            fi
        done

        # Check for extra owners
        local crate_extra=()
        while IFS= read -r owner; do
            [ -z "$owner" ] && continue
            local is_required=false
            for required in "${REQUIRED_OWNERS[@]}"; do
                if [ "$owner" = "$required" ]; then
                    is_required=true
                    break
                fi
            done
            if [ "$is_required" = false ]; then
                crate_extra+=("$owner")
            fi
        done <<< "$owners"

        if [ ${#crate_missing[@]} -gt 0 ] || [ ${#crate_extra[@]} -gt 0 ]; then
            echo "ISSUES FOUND"
            if [ ${#crate_missing[@]} -gt 0 ]; then
                missing_owners+=("${crate}: missing ${crate_missing[*]}")
            fi
            if [ ${#crate_extra[@]} -gt 0 ]; then
                extra_owners+=("${crate}: unexpected ${crate_extra[*]}")
            fi
        else
            echo "ok"
        fi

        # Rate limit: crates.io asks for max 1 req/sec
        sleep 1
    done

    # Write GitHub Actions job summary if available
    local summary=""

    summary+="## Crate Ownership Check\n\n"

    if [ ${#not_published[@]} -gt 0 ]; then
        summary+="### Not yet published on crates.io (${#not_published[@]})\n\n"
        for entry in "${not_published[@]}"; do
            summary+="- \`${entry}\`\n"
        done
        summary+="\n"
    fi

    if [ ${#missing_owners[@]} -gt 0 ]; then
        summary+="### :x: Crates with missing required owners (${#missing_owners[@]})\n\n"
        for entry in "${missing_owners[@]}"; do
            summary+="- ${entry}\n"
        done
        summary+="\n"
        errors=1
    fi

    if [ ${#extra_owners[@]} -gt 0 ]; then
        summary+="### :x: Crates with unexpected additional owners (${#extra_owners[@]})\n\n"
        for entry in "${extra_owners[@]}"; do
            summary+="- ${entry}\n"
        done
        summary+="\n"
        errors=1
    fi

    if [ $errors -eq 0 ] && [ ${#not_published[@]} -eq 0 ]; then
        summary+="### :white_check_mark: All published crates have correct ownership.\n"
    fi

    # Print to stdout
    echo ""
    printf '%b' "$summary"

    # Write to GitHub Actions job summary if running in CI
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '%b' "$summary" >> "$GITHUB_STEP_SUMMARY"
    fi

    return $errors
}

main "$@"
