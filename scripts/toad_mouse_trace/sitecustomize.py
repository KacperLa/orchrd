"""Opt-in, event-only Textual trace for the trace-toad-mouse launcher."""

import builtins
import os
from datetime import datetime, timezone


_trace_file = os.environ.get("TOAD_MOUSE_TRACE_FILE")
_original_import = builtins.__import__
_installed = False


def _write(*fields):
    if not _trace_file:
        return
    line = " ".join(str(field) for field in fields)
    try:
        fd = os.open(_trace_file, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        try:
            os.write(fd, (line + "\n").encode("utf-8", errors="replace"))
        finally:
            os.close(fd)
    except OSError:
        pass


def _widget_name(widget):
    if widget is None:
        return "none"
    return f"{type(widget).__name__}#{getattr(widget, 'id', None) or '-'}"


def _install():
    global _installed
    if _installed:
        return
    _installed = True
    try:
        from textual import events
        from textual.message_pump import MessagePump

        mouse_types = (
            events.MouseDown,
            events.MouseUp,
            events.Click,
            events.MouseScrollDown,
            events.MouseScrollUp,
        )
        original_dispatch = MessagePump._dispatch_message

        async def traced_dispatch(self, message):
            if isinstance(message, mouse_types):
                app = getattr(self, "app", None)
                screen = getattr(app, "screen", None) if app is not None else None
                focused = getattr(screen, "focused", None)
                timestamp = datetime.now(timezone.utc).isoformat(timespec="milliseconds")
                _write(
                    timestamp,
                    type(message).__name__,
                    f"receiver={_widget_name(self)}",
                    f"x={getattr(message, 'x', '-')}",
                    f"y={getattr(message, 'y', '-')}",
                    f"forwarded={getattr(message, 'is_forwarded', '-')}",
                    f"focus={_widget_name(focused)}",
                )
            return await original_dispatch(self, message)

        MessagePump._dispatch_message = traced_dispatch
        _write(datetime.now(timezone.utc).isoformat(timespec="seconds"), "trace=ready")
    except Exception as error:
        _write(datetime.now(timezone.utc).isoformat(timespec="seconds"), "trace=failed", type(error).__name__)


def _traced_import(name, globals=None, locals=None, fromlist=(), level=0):
    result = _original_import(name, globals, locals, fromlist, level)
    if name == "toad.cli":
        _install()
        builtins.__import__ = _original_import
    return result


if _trace_file:
    builtins.__import__ = _traced_import
