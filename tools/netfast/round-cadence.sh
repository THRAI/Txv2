#!/bin/sh
# Poll a witness serial log once per second and print "epoch round-count"
# whenever the per-round marker count changes. Gives per-round cadence for
# logs that carry no timestamps.
# Usage: round-cadence.sh <log> <marker-regex> [max-seconds]
LOG="${1:?log}"; PAT="${2:?marker regex}"; MAX="${3:-600}"
last=-1; i=0
while [ "$i" -lt "$MAX" ]; do
    if [ -f "$LOG" ]; then
        n=$(grep -ac "$PAT" "$LOG" 2>/dev/null || echo 0)
        if [ "$n" != "$last" ]; then
            echo "$(date +%s) $n"
            last="$n"
        fi
        case "$(tail -c 2000 "$LOG" 2>/dev/null)" in
            *"GROUP END"*) echo "$(date +%s) END"; exit 0 ;;
        esac
    fi
    sleep 1; i=$((i+1))
done
