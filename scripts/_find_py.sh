#!/usr/bin/env bash
set -euo pipefail
for d in /nix/store/*-python3.13-with-packages*/; do
  if ls "$d/lib/python3.13/site-packages" 2>/dev/null | grep -q pycairo && ls "$d/lib/python3.13/site-packages" 2>/dev/null | grep -q gi; then
    echo "$d"
  fi
done 2>/dev/null | head -3
