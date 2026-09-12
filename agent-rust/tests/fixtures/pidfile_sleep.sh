#!/bin/sh
# Publishes its own pid, then becomes the sleeping process itself (exec, so the pid stays
# valid) for as long as any reasonable test timeout.
echo $$ > "$1"
exec sleep 10
