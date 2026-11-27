#!/bin/sh
# Fake `hc connect --json` for herdr-runtime tests.
#
# NDJSON JSON-RPC responder driven by $FAKE_HC_DIR:
#   resp/<method>.json      -> result body (raw JSON, wrapped by us)
#   resp/<method>.err       -> error envelope body
#   resp/<method>.<n>.json/.err -> nth-call overrides (count/<method>)
#   event.json              -> extra stdout lines emitted after an
#                            events.subscribe result (event notifications)
#   seen.log                -> every request line received
#
# Missing responses default to {"result":null}.
set -u

dir="${FAKE_HC_DIR:?FAKE_HC_DIR not set}"
mkdir -p "$dir/resp" "$dir/count"

while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in *'"id":'*) ;; *) continue ;; esac
    printf '%s\n' "$line" >> "$dir/seen.log"

    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
    [ -z "$id" ] || [ -z "$method" ] && continue

    ctr="$dir/count/$method"
    n=0
    [ -f "$ctr" ] && n=$(cat "$ctr")
    n=$((n + 1))
    printf '%s' "$n" > "$ctr"

    base="$dir/resp/$method"
    body=""
    err=""
    if [ -f "$base.$n.err" ]; then err=$(cat "$base.$n.err");
    elif [ -f "$base.err" ]; then err=$(cat "$base.err");
    elif [ -f "$base.$n.json" ]; then body=$(cat "$base.$n.json");
    elif [ -f "$base.json" ]; then body=$(cat "$base.json");
    else body="null"; fi

    if [ -n "$err" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":%s}\n' "$id" "$err"
    else
        printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$body"
    fi

    if [ "$method" = "events.subscribe" ] && [ -f "$dir/event.json" ]; then
        cat "$dir/event.json"
    fi
done
