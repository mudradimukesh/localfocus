#!/usr/bin/env sh
set -eu

cargo install --path .
echo "Installed local-focus. Run: local-focus serve"
