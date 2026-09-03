#!/bin/sh
set -eu

SOURCE_PEER=${1:?source Peer number is required}
TARGET_PEER=${2:?target Peer number is required}
EXPECTED_PATH=${3:?expected path is required}
case "$SOURCE_PEER:$TARGET_PEER:$EXPECTED_PATH" in
    [123]:[123]:P2p|[123]:[123]:Relay) ;;
    *) echo "invalid wait-path arguments" >&2; exit 2 ;;
esac

LOG_FILE="/state/client${SOURCE_PEER}/client.log"
test -f "$LOG_FILE"
START_LINE=$(wc -l < "$LOG_FILE")
START_LINE=$((START_LINE + 1))

ATTEMPT=1
while [ "$ATTEMPT" -le 45 ]; do
    if /bin/sh /accept/probe.sh "$TARGET_PEER"; then
        if tail -n "+$START_LINE" "$LOG_FILE" \
            | grep 'selected replica lane for TCP open' \
            | grep -q "path=$EXPECTED_PATH" \
            && tail -n "+$START_LINE" "$LOG_FILE" \
                | grep 'selected replica lane for UDP open' \
                | grep -q "path=$EXPECTED_PATH"; then
            echo "Peer ${SOURCE_PEER} -> Peer ${TARGET_PEER}: TCP+UDP path=${EXPECTED_PATH} PASS"
            exit 0
        fi
    fi
    ATTEMPT=$((ATTEMPT + 1))
    sleep 2
done

echo "timed out waiting for Peer ${SOURCE_PEER} -> Peer ${TARGET_PEER} path=${EXPECTED_PATH}" >&2
tail -n "+$START_LINE" "$LOG_FILE" >&2
exit 1

