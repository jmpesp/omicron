#!/usr/bin/env bash
if [ "$(uname)" = "Darwin" ]; then
    while [ 1 ]; do
        echo "*** $(memory_pressure | tail -1)"
        sleep 5;
    done
fi
