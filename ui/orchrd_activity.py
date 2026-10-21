#!/usr/bin/env python3
"""Native GTK4 renderer for coordinator snapshots. No log parsing or state inference."""
import json
import math
import sys
import time

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Gio, GLib, Gtk, Gdk, Pango, PangoCairo
from orchrd_activity_render import ACTION_GLYPHS, draw_glyph, COLORS

BUS = "io.github.kacper.Orchrd1"
PATH = "/io/github/kacper/Orchrd1"
LANE_SPACING = 128
LANE_ORIGIN = LANE_SPACING / 2
ROW_SPACING = 72
LABEL_WIDTH = LANE_SPACING - 12

def action_title(kind):
    return ACTION_GLYPHS.get(kind, ("unknown", kind.replace("_", " ")))[1]

def lane_names(snapshot):
    names = [a["agent_name"] for a in snapshot.get("agents", [])]
    for item in snapshot.get("activity", []):
        name = item.get("agent_name")
        # Retain departed lanes only when Toad actually reported the agent.
        # A model's attempted target is not evidence that an agent exists.
        if item["kind"] == "agent_status" and name and name not in names:
            names.append(name)
    return [None] + names

def lane_for(item, lanes):
    name = item.get("agent_name")
    return None if item["kind"] in ("utterance", "summary", "speech", "monitor") or name not in lanes else name

def lane_label(snapshot, identifier):
    if identifier is None:
        return "Orchrd"
    agents = snapshot.get("agents", [])
    agent = next((a for a in agents if a["agent_name"] == identifier), None)
    if agent is None:
        return identifier
    title = agent.get("display_name") or identifier
    if sum((a.get("display_name") or a["agent_name"]) == title for a in agents) > 1:
        return f"{title} ({identifier})"
    return title

class ActivityWindow(Gtk.ApplicationWindow):
    def __init__(self, app):
        super().__init__(application=app, title="Orchrd · Activity", default_width=1180, default_height=760)
        self.snapshot = {}
        self.proxy = None
        self.connected = False
        self.generation = 0
        self.selected = None
        self.points = []
        self.closed = False
        self.last_frame = 0
        self.syncing = False
        header = Gtk.HeaderBar()
        header.set_title_widget(Gtk.Label(label="Orchrd / Live activity"))
        self.set_titlebar(header)
        self.follow = Gtk.ToggleButton(label="Follow", active=True)
        header.pack_end(self.follow)
        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        for edge in ("top", "bottom", "start", "end"):
            getattr(root, f"set_margin_{edge}")(12)
        self.set_child(root)
        self.status = Gtk.Label(label="Connecting to Orchrd…", xalign=0)
        root.append(self.status)
        self.questions = Gtk.Label(xalign=0, wrap=True, selectable=True)
        self.questions_panel = Gtk.Expander(label="Agent questions")
        self.questions_panel.set_child(self.questions)
        self.questions_panel.set_expanded(True)
        root.append(self.questions_panel)
        legend = Gtk.Label(label="Blue / working    Gray / idle    Green / done    Amber / blocked or uncertain    Red / failed", xalign=0)
        legend.add_css_class("dim-label")
        root.append(legend)
        split = Gtk.Paned(orientation=Gtk.Orientation.HORIZONTAL, vexpand=True)
        root.append(split)
        self.scroll = Gtk.ScrolledWindow(hexpand=True, vexpand=True)
        self.canvas = Gtk.DrawingArea(content_width=256, content_height=480, focusable=True)
        self.canvas.set_draw_func(self.draw)
        self.canvas.set_has_tooltip(True)
        self.canvas.connect("query-tooltip", self.node_tooltip)
        click = Gtk.GestureClick()
        click.connect("pressed", self.clicked)
        self.canvas.add_controller(click)
        keys = Gtk.EventControllerKey()
        keys.connect("key-pressed", self.key_pressed)
        self.canvas.add_controller(keys)
        self.scroll.set_child(self.canvas)
        graph = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        graph.append(self.scroll)
        # Outside the vertical scroller; only its horizontal offset follows lanes.
        self.footer = Gtk.DrawingArea(content_height=46, hexpand=True)
        self.footer.set_draw_func(self.draw_footer)
        graph.append(self.footer)
        self.scroll.get_hadjustment().connect("value-changed", lambda *_: self.footer.queue_draw())
        split.set_start_child(graph)
        sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8, width_request=330)
        sidebar.set_margin_start(16)
        self.inspector_title = Gtk.Label(label="Inspect an operation", xalign=0)
        sidebar.append(self.inspector_title)
        self.choices = Gtk.DropDown(model=Gtk.StringList.new([]))
        self.choices.set_tooltip_text("Activity nodes, in coordinator order")
        self.choices.connect("notify::selected", self.choose)
        sidebar.append(self.choices)
        self.detail = Gtk.TextView(editable=False, cursor_visible=False, monospace=True, wrap_mode=Gtk.WrapMode.WORD_CHAR)
        detail_scroll = Gtk.ScrolledWindow(vexpand=True)
        detail_scroll.set_child(self.detail)
        sidebar.append(detail_scroll)
        split.set_end_child(sidebar)
        split.set_position(790)
        self.connect("close-request", self.close)
        self.tick = self.canvas.add_tick_callback(self.animate)
        Gio.DBusProxy.new_for_bus(Gio.BusType.SESSION, Gio.DBusProxyFlags.DO_NOT_AUTO_START,
            None, BUS, PATH, BUS, None, self.proxy_ready)

    def proxy_ready(self, _source, result):
        if self.closed:
            return
        try:
            self.proxy = Gio.DBusProxy.new_for_bus_finish(result)
            self.proxy.connect("g-signal", self.signal)
            self.proxy.connect("notify::g-name-owner", self.owner_changed)
            self.owner_changed()
        except GLib.Error as error:
            self.status.set_text(f"Disconnected: {error.message}")

    def owner_changed(self, *_args):
        if self.closed:
            return
        self.generation += 1
        self.connected = bool(self.proxy.get_name_owner())
        self.snapshot = {}
        self.selected = None
        self.render()
        if self.connected:
            generation = self.generation
            def received(proxy, result):
                try:
                    raw = proxy.call_finish(result).unpack()[0]
                    if not self.closed and generation == self.generation:
                        self.accept(raw)
                except (GLib.Error, ValueError, KeyError) as error:
                    if not self.closed and generation == self.generation:
                        self.status.set_text(f"Snapshot unavailable: {error}")
            self.proxy.call("GetSnapshot", None, Gio.DBusCallFlags.NONE, 5000, None, received)

    def signal(self, _proxy, _sender, name, parameters):
        if not self.closed and name == "StateChanged":
            try:
                self.accept(parameters.unpack()[0])
            except (ValueError, KeyError) as error:
                self.status.set_text(f"Invalid snapshot: {error}")

    def accept(self, raw):
        snapshot = json.loads(raw)
        if snapshot.get("revision", 0) < self.snapshot.get("revision", -1):
            return
        self.snapshot = snapshot
        self.render()

    def render(self):
        self.syncing = True
        requests = []
        for value in self.snapshot.get("observed_questions", []):
            question = value["question"]
            lines = [lane_label(self.snapshot, value["agent_name"]), question["body"]]
            lines.extend(f"{index + 1}. {option['label']}" for index, option in enumerate(question.get("options", [])))
            lines.append("Answer in the terminal; controls are not established." if value.get("read_only") else "Reply by voice or in the agent terminal.")
            requests.append("\n".join(lines))
        self.questions.set_text("\n\n".join(requests))
        self.questions_panel.set_visible(bool(requests))
        records = self.snapshot.get("activity", [])
        self.choices.set_model(Gtk.StringList.new([f'{action_title(a["kind"])} · {a["state"]} · {lane_label(self.snapshot, a.get("agent_name"))}' for a in records]))
        index = next((i for i, a in enumerate(records) if a["id"] == self.selected), None)
        self.choices.set_selected(index if index is not None else Gtk.INVALID_LIST_POSITION)
        self.syncing = False
        monitor = next((a for a in records if a["id"] == "monitor"), None)
        health = f' · Toad connection: {monitor["state"]}' if monitor else ""
        self.status.set_text(f'{len(self.snapshot.get("agents", []))} connected agents · {self.snapshot.get("state", "connecting")}{health} · recent activity (this service lifetime)' if self.connected else "Orchrd is offline — waiting for the service")
        self.canvas.set_content_width(max(256, len(lane_names(self.snapshot)) * LANE_SPACING))
        self.canvas.set_content_height(max(440, 180 + len(records) * ROW_SPACING))
        self.show_detail()
        self.canvas.queue_draw()
        self.footer.set_tooltip_text(" · ".join(lane_label(self.snapshot, name) for name in lane_names(self.snapshot)))
        self.footer.queue_draw()
        if self.follow.get_active():
            GLib.idle_add(self.scroll_to_end)

    def scroll_to_end(self):
        if not self.closed:
            adjustment = self.scroll.get_vadjustment()
            adjustment.set_value(max(0, adjustment.get_upper() - adjustment.get_page_size()))
        return GLib.SOURCE_REMOVE

    def choose(self, *_args):
        if self.syncing:
            return
        records = self.snapshot.get("activity", [])
        index = self.choices.get_selected()
        if index < len(records):
            self.selected = records[index]["id"]
            self.show_detail()
            self.canvas.queue_draw()

    def show_detail(self):
        item = next((a for a in self.snapshot.get("activity", []) if a["id"] == self.selected), None)
        if item is None:
            item = next((a for a in self.snapshot.get("agents", []) if f'agent:{a["agent_name"]}' == self.selected), None)
        text = json.dumps(item, indent=2, ensure_ascii=False) if item else "Select a node to see the exact transcript, tool arguments, delivery receipt, output, or error.\n\nEdges show explicit parent IDs. Agent endpoints use Toad status, never delivery status."
        self.detail.get_buffer().set_text(text)
        self.inspector_title.set_text(action_title(item["kind"]) if item and "kind" in item else "Agent details" if item else "Inspect an operation")

    def node_tooltip(self, _canvas, x, y, keyboard, tooltip):
        identifier = self.selected if keyboard else next((identifier for px, py, identifier in self.points if math.hypot(px-x, py-y) <= 21), None)
        item = next((a for a in self.snapshot.get("activity", []) if a["id"] == identifier), None)
        if item:
            tooltip.set_text(f'{action_title(item["kind"])} · {item["state"]}')
            return True
        if identifier and identifier.startswith("agent:"):
            tooltip.set_text("Current stream status")
            return True
        return False

    def clicked(self, _gesture, _press, x, y):
        self.canvas.grab_focus()
        for px, py, identifier in self.points:
            if math.hypot(px-x, py-y) <= 21:
                self.selected = identifier
                self.show_detail()
                self.canvas.queue_draw()
                break

    def key_pressed(self, _controller, key, _code, _mods):
        records = self.snapshot.get("activity", [])
        if not records or key not in (Gdk.KEY_Up, Gdk.KEY_Down, Gdk.KEY_End, Gdk.KEY_Home):
            return False
        index = next((i for i, a in enumerate(records) if a["id"] == self.selected), -1)
        index = len(records) - 1 if key == Gdk.KEY_End else 0 if key == Gdk.KEY_Home else max(0, min(len(records)-1, index + (1 if key == Gdk.KEY_Down else -1)))
        self.choices.set_selected(index)
        return True

    def animate(self, *_args):
        if self.closed:
            return False
        now = GLib.get_monotonic_time()
        if self.get_mapped() and now - self.last_frame > 66000:
            self.last_frame = now
            self.canvas.queue_draw()
        return True

    def footer_labels(self):
        offset = self.scroll.get_hadjustment().get_value()
        return [(LANE_ORIGIN + i * LANE_SPACING - offset, lane_label(self.snapshot, name))
                for i, name in enumerate(lane_names(self.snapshot))]

    def draw_footer(self, _area, cr, width, height):
        cr.set_source_rgb(0.10, 0.12, 0.15)
        cr.paint()
        cr.set_source_rgb(0.25, 0.29, 0.34)
        cr.set_line_width(1)
        cr.move_to(0, 0.5)
        cr.line_to(width, 0.5)
        cr.stroke()
        for x, text in self.footer_labels():
            self.draw_label(cr, x, 14, text, centered=True)

    @staticmethod
    def draw_label(cr, x, y, text, color=(0.80, 0.84, 0.89), centered=False):
        cr.set_source_rgb(*color)
        layout = PangoCairo.create_layout(cr)
        layout.set_font_description(Pango.FontDescription("Sans 10"))
        layout.set_width(LABEL_WIDTH * Pango.SCALE)
        if centered:
            layout.set_alignment(Pango.Alignment.CENTER)
            x -= LABEL_WIDTH / 2
        layout.set_ellipsize(Pango.EllipsizeMode.END)
        layout.set_text(text, -1)
        cr.move_to(x, y)
        PangoCairo.show_layout(cr, layout)

    def draw_status(self, cr, x, node_y, text, color=(0.80, 0.84, 0.89)):
        # Keep the lane line from running through the centered caption.
        cr.save()
        cr.set_source_rgb(0.075, 0.085, 0.10)
        cr.rectangle(x - LABEL_WIDTH / 2, node_y + 22, LABEL_WIDTH, 22)
        cr.fill()
        self.draw_label(cr, x, node_y + 24, text, color, centered=True)
        cr.restore()

    def draw(self, _area, cr, width, height):
        cr.set_source_rgb(0.075, 0.085, 0.10)
        cr.paint()
        lanes = lane_names(self.snapshot)
        positions = {name: LANE_ORIGIN + i * LANE_SPACING for i, name in enumerate(lanes)}
        records = self.snapshot.get("activity", [])
        bottom = max(330, 105 + len(records) * ROW_SPACING)
        now = time.time()
        self.points = []
        def label(x, y, text, color=(0.80, 0.84, 0.89)):
            self.draw_label(cr, x, y, text, color)
        for name, x in positions.items():
            cr.set_source_rgb(0.20, 0.24, 0.29)
            cr.set_line_width(2)
            cr.move_to(x, 44)
            cr.line_to(x, bottom)
            cr.stroke()
        points = {a["id"]: (positions[lane_for(a, lanes)], 75 + i * ROW_SPACING) for i, a in enumerate(records)}
        for item in records:
            x, y = points[item["id"]]
            parent = points.get(item.get("parent"))
            if parent:
                px, py = parent
                cr.set_source_rgba(0.40, 0.69, 1.0, 0.40)
                cr.move_to(px, py)
                cr.curve_to(px, (py+y)/2, x, (py+y)/2, x, y)
                cr.stroke()
        # Paint nodes and captions above all connecting edges.
        for item in records:
            x, y = points[item["id"]]
            state = item["state"]
            color = COLORS.get(state, (0.40, 0.69, 1.0) if not item["finished"] else (0.63, 0.67, 0.73))
            self.node(cr, x, y, color, not item["finished"], False, now, item["id"], item["kind"])
            elapsed = max(0, ((item["updated_ms"] if item["finished"] else now*1000) - item["started_ms"]) / 1000)
            self.draw_status(cr, x, y, f'{state} · {elapsed:.0f}s', color)
        agents = {a["agent_name"]: a for a in self.snapshot.get("agents", [])}
        for name, x in positions.items():
            state = self.snapshot.get("voice_state", "idle") if name is None else agents.get(name, {}).get("status", "not present")
            busy = state in ("listening", "transcribing", "routing") if name is None else state == "working"
            self.node(cr, x, bottom, COLORS.get(state, (0.48, 0.51, 0.56)), busy, state == "done", now, f"agent:{name}", "utterance" if name is None else "agent_status")
            self.draw_status(cr, x, bottom, state)
        if not records:
            label(65, 100, "Waiting for live activity")

    def node(self, cr, x, y, color, busy, pulse, now, identifier, kind):
        if not self.get_settings().get_property("gtk-enable-animations"):
            pulse = False
            now = 0
        if pulse:
            cr.set_source_rgba(*color, 0.25)
            cr.arc(x, y, 18 + 2 * math.sin(now*3), 0, math.tau)
            cr.fill()
        cr.set_source_rgb(0.10, 0.12, 0.15)
        cr.arc(x, y, 15, 0, math.tau)
        cr.fill()
        cr.set_source_rgb(*color)
        cr.set_line_width(2)
        cr.arc(x, y, 15, 0, math.tau)
        cr.stroke()
        if busy or identifier == self.selected:
            cr.set_line_width(2)
            start = now * 4 if busy else 0
            cr.arc(x, y, 20, start, start + (4.5 if busy else math.tau))
            cr.stroke()
        draw_glyph(cr, kind, x, y)
        self.points.append((x, y, identifier))

    def close(self, *_args):
        self.closed = True
        self.canvas.remove_tick_callback(self.tick)
        if self.proxy:
            self.proxy.disconnect_by_func(self.signal)
            self.proxy.disconnect_by_func(self.owner_changed)
        return False

class App(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="io.github.kacper.Orchrd.Activity")

    def do_activate(self):
        window = self.get_active_window()
        if window is None:
            window = ActivityWindow(self)
        window.present()

if __name__ == "__main__":
    App().run(sys.argv)
