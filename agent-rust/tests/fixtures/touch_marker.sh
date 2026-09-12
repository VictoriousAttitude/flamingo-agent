#!/bin/sh
# Records that this child ran. The agent always passes the child log path last
# (--log-file <path>), so the last argument is used as the marker; the directory is
# created first so the marker appears even when the agent never secured the log.
eval "marker=\${$#}"
mkdir -p "$(dirname "$marker")"
: > "$marker"
