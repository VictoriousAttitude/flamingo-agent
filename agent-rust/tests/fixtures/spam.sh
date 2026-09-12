#!/bin/sh
# 300 kB on stdout: more than any pipe buffer holds.
head -c 300000 /dev/zero | tr '\0' 'x'
