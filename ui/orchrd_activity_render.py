#!/usr/bin/env python3
"""Native GTK4 drawing primitives for the activity view.

Kept separate from orchrd_activity.py so the vector glyphs and status palette
can be unit-tested and reused without pulling in the D-Bus window.
"""
import math

ACTION_GLYPHS = {
    "utterance": ("microphone", "User utterance"),
    "list_agents": ("list", "List agents"),
    "read_agent": ("eye", "Read agent"),
    "send_to_agent": ("plane", "Send request"),
    "delivery": ("inbox", "Delivery / confirmation"),
    "respond_to_agent": ("shield", "Approve or deny"),
    "cancel_agent": ("stop", "Cancel agent"),
    "summary": ("document", "Summarize output"),
    "dialog": ("question", "Read question"),
    "speech": ("speaker", "Speak"),
    "agent_status": ("pulse", "Agent status transition"),
    "monitor": ("radar", "Toad monitoring"),
    "latest_result": ("history", "Latest result"),
}


def draw_glyph(cr, kind, x, y):
    """Small code-native vector glyphs: no font/emoji or icon-theme dependency."""
    glyph = ACTION_GLYPHS.get(kind, ("unknown", ""))[0]
    cr.save()
    cr.translate(x - 9, y - 9)
    cr.scale(0.75, 0.75)
    cr.set_source_rgb(0.95, 0.97, 1.0)
    cr.set_line_width(1.8)
    cr.set_line_cap(1)
    cr.set_line_join(1)

    def line(*points):
        cr.move_to(*points[0])
        for point in points[1:]:
            cr.line_to(*point)
        cr.stroke()

    def circle(cx, cy, radius):
        cr.new_sub_path()
        cr.arc(cx, cy, radius, 0, math.tau)
        cr.stroke()

    def rect(left, top, width, height):
        cr.rectangle(left, top, width, height)
        cr.stroke()

    if glyph == "microphone":
        cr.arc(12, 6, 3, math.pi, math.tau)
        cr.line_to(15, 12)
        cr.arc(12, 12, 3, 0, math.pi)
        cr.close_path()
        cr.stroke()
        cr.arc(12, 11, 7, 0, math.pi)
        cr.stroke()
        line((12, 18), (12, 22))
        line((8, 22), (16, 22))
    elif glyph == "list":
        for row in (6, 12, 18):
            line((4, row), (5, row))
            line((9, row), (21, row))
    elif glyph == "eye":
        cr.move_to(2, 12)
        cr.curve_to(7, 2, 17, 2, 22, 12)
        cr.curve_to(17, 22, 7, 22, 2, 12)
        cr.stroke()
        circle(12, 12, 3)
    elif glyph == "question":
        rect(3, 3, 18, 14)
        line((7, 17), (7, 22), (12, 17))
        line((10, 7), (14, 7), (14, 10), (12, 10), (12, 12))
        circle(12, 14, 0.5)
    elif glyph == "plane":
        line((3, 10), (22, 2), (14, 22), (10, 14), (3, 10))
        line((10, 14), (22, 2))
    elif glyph == "inbox":
        line((3, 12), (3, 21), (21, 21), (21, 12))
        line((3, 15), (8, 15), (10, 18), (14, 18), (16, 15), (21, 15))
        line((12, 2), (12, 12))
        line((8, 8), (12, 12), (16, 8))
    elif glyph == "shield":
        line((12, 2), (21, 6), (19, 16), (12, 22), (5, 16), (3, 6), (12, 2))
    elif glyph == "stop":
        rect(5, 5, 14, 14)
    elif glyph in ("document", "history"):
        line((14, 2), (4, 2), (4, 22), (20, 22), (20, 8), (14, 2), (14, 8), (20, 8))
        if glyph == "document":
            for row in (12, 16):
                line((8, row), (16, row))
        else:
            circle(12, 15, 5)
            line((12, 12), (12, 15), (15, 16))
    elif glyph == "speaker":
        line((3, 9), (7, 9), (12, 4), (12, 20), (7, 15), (3, 15), (3, 9))
        for radius in (5, 9):
            cr.new_sub_path()
            cr.arc(12, 12, radius, -0.8, 0.8)
            cr.stroke()
    elif glyph == "pulse":
        line((2, 12), (6, 12), (9, 3), (14, 21), (17, 12), (22, 12))
    elif glyph == "radar":
        circle(12, 12, 9)
        circle(12, 12, 4)
        line((12, 12), (19, 5))
    else:
        circle(12, 12, 7)
        line((12, 7), (12, 13))
        circle(12, 17, 0.5)
    cr.restore()


# Status -> accent color, matched to the palette used across the activity view.
COLORS = {
    "working": (0.40, 0.69, 1.0), "done": (0.35, 0.85, 0.57),
    "idle": (0.48, 0.51, 0.56), "blocked": (1.0, 0.72, 0.30),
    "failed": (1.0, 0.40, 0.42), "unconfirmed": (1.0, 0.72, 0.30),
}
