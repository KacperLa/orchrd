#!/usr/bin/env bash
set -euo pipefail
rm -f /tmp/mydbus.sock
/nix/store/0rhdz2wgvik5bcrlhlq8nfh1c3y04vv3-system-path/bin/dbus-daemon \
    --session --address="unix:path=/tmp/mydbus.sock" \
    --nofork --print-address > /tmp/dbus_addr.txt 2>/tmp/dbus_err.txt &
sleep 2
echo "ADDR:"; cat /tmp/dbus_addr.txt
echo "ERR:"; cat /tmp/dbus_err.txt
