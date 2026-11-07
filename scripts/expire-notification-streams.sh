#!/usr/bin/env bash
# Backfill TTLs for notification streams created before idle expiry existed.
# Dry-run by default. Pass --apply first to attach run/task TTLs; no keys
# are deleted and existing TTLs are left alone. Remaining args go to redis-cli.
# Use REDISCLI_AUTH for authentication. On a cluster run against each primary:
# SCAN only visits the connected node. Queue and environment streams are excluded.
set -euo pipefail

apply=0
if [[ "${1:-}" == "--help" ]]; then
    echo "Usage: $0 [--apply] [redis-cli connection options]"
    exit 0
fi
if [[ "${1:-}" == "--apply" ]]; then
    apply=1
    shift
fi

stream_keys=$(mktemp)
trap 'rm -f "$stream_keys"' EXIT
redis-cli "$@" --scan --pattern 'hot:stream:*' > "$stream_keys"

# Check type and TTL in the same operation as the update so a concurrent
# publish cannot have its renewed TTL shortened by this maintenance pass.
script="
if redis.call('TYPE', KEYS[1]).ok ~= 'stream' or redis.call('TTL', KEYS[1]) ~= -1 then
    return 0
end
local ttl = ARGV[1]
local last = redis.call('XREVRANGE', KEYS[1], '+', '-', 'COUNT', 1)
if #last > 0 then
    local fields = last[1][2]
    for i = 1, #fields, 2 do
        if fields[i] == 'event' then
            local ok, event = pcall(cjson.decode, fields[i + 1])
            if ok and event.type == 'task:message' then ttl = 691200 end
        end
    end
end
if ARGV[2] == '1' then
    return redis.call('EXPIRE', KEYS[1], ttl)
end
return 1"

count=0
while IFS= read -r key; do
    result=$(redis-cli "$@" --raw EVAL "$script" 1 "$key" "${HOT_RUN_STREAM_IDLE_TTL_SECONDS:-7200}" "$apply")
    case "$result" in
        1) count=$((count + 1)) ;;
        0) ;;
        *) echo "Redis TTL backfill failed: $result" >&2; exit 1 ;;
    esac
done < "$stream_keys"

if [[ "$apply" == 1 ]]; then
    echo "Attached run/task TTLs to $count persistent notification streams."
else
    echo "Found $count persistent notification streams; pass --apply to attach run/task TTLs."
fi
