#!/usr/bin/env bash
set -euo pipefail
cd /home/kacper/Documents/Projects/orchrd
PY=/nix/store/a40cmm6gx8fjxr5myh7prz7wadr25wix-python3-3.13.15-env/bin/python3
GTK=/nix/store/0qvbk3h6354qr5dy7n2i28khlfi6fzm6-gtk4-4.22.4
export PATH="/nix/store/iyzy0zy23i1njxar2sd1hr4hmk56dalh-xvfb-run-1+g87f6705/bin:${PATH}"
export PYTHONPATH=""
export PYTHONHOME=""
export GI_TYPELIB_PATH="${GTK}/lib/girepository-1.0"
export XDG_DATA_DIRS="${GTK}/share"
export GDK_BACKEND=x11
export XDG_RUNTIME_DIR=/tmp/orchrd-runtime
export GTK_A11Y=none
export GIO_USE_VFS=local
export GTK_USE_PORTAL=0
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
xvfb-run -a dbus-run-session --config-file=ui/test-bus.conf \
    "$PY" -m unittest ui.test_activity -v
