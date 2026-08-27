#!/usr/bin/env bash
# Print the timestamp/hash identity shared by builds, archives, and tags.

set -euo pipefail

commit=${TERMNAV_BUILD_COMMIT:-}
if [[ -z $commit ]]; then
  commit=$(git rev-parse HEAD)
fi
case $commit in
  *[!0-9A-Fa-f]* | ??????? | ?????? | ????? | ???? | ??? | ?? | ? | '')
    printf 'invalid Termnav build commit: %s\n' "$commit" >&2
    exit 1
    ;;
esac

timestamp=${TERMNAV_BUILD_TIMESTAMP:-}
if [[ -z $timestamp ]]; then
  timestamp=$(git show -s --format=%ct "$commit")
fi
case $timestamp in
  *[!0-9]* | '')
    printf 'invalid Termnav build timestamp: %s\n' "$timestamp" >&2
    exit 1
    ;;
esac

if date -u -d "@$timestamp" +%Y%m%d-%H%M%S >/dev/null 2>&1; then
  date_utc=$(date -u -d "@$timestamp" +%Y%m%d-%H%M%S)
else
  date_utc=$(date -u -r "$timestamp" +%Y%m%d-%H%M%S)
fi
printf '%s-%s\n' "$date_utc" "${commit:0:8}"
