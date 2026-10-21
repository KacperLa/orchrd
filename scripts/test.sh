#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 0 ]]; then
    echo "Usage: $0" >&2
    exit 2
fi

if ! command -v nix >/dev/null 2>&1; then
    echo "Nix with flakes enabled is required to run the tests." >&2
    exit 1
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repo_root"

# Always enter the pinned shell, even from another project's development shell.
# The commands run from the working tree, including uncommitted source changes.
exec nix develop "path:$repo_root" --command bash -euo pipefail -c '
    export PYTHONDONTWRITEBYTECODE=1
    export ORCHRD_SKIP_CALLGRAPH=1

    echo "Checking Rust formatting..."
    cargo fmt --all -- --check

    echo "Running Rust tests..."
    cargo test --locked --all-targets

    echo "Running Clippy..."
    cargo clippy --locked --all-targets -- -D warnings

    echo "Running GNOME extension tests..."
    env -u GI_TYPELIB_PATH -u GIO_EXTRA_MODULES -u GIO_MODULE_DIR \
        gjs extension/test-lifecycle.js

    echo "Running Apple TV remote tests..."
    env -u PYTHONPATH -u PYTHONHOME \
        python3 -m unittest discover -s apple_tv_remote

    echo "Running TTS worker tests..."
    env -u PYTHONPATH -u PYTHONHOME \
        "$ORCHRD_TEST_PYTHON" -m unittest discover -s tts

    echo "Running GTK viewer tests in an isolated display and D-Bus session..."
    env -u GIO_EXTRA_MODULES -u GIO_MODULE_DIR -u PYTHONPATH -u PYTHONHOME \
        GDK_BACKEND=x11 XDG_DATA_DIRS="$ORCHRD_TEST_GTK_DATA" \
        GTK_A11Y=none GIO_USE_VFS=local GTK_USE_PORTAL=0 \
        xvfb-run -a dbus-run-session --config-file=ui/test-bus.conf -- \
        "$ORCHRD_TEST_PYTHON" -m unittest discover -s ui

    echo "All checks passed."
'
