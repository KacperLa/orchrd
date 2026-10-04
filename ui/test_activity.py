import json
import unittest
import os
import time
from unittest.mock import Mock, patch
import cairo
from orchrd_activity import ActivityWindow, Gtk, Gio, GLib, lane_names, lane_for, lane_label, BUS, PATH
from orchrd_activity import action_title
from orchrd_activity_render import ACTION_GLYPHS, draw_glyph


def fixture():
    records = []
    for i, (kind, state, name, parent, finished) in enumerate([
        ("utterance", "executing_tools", None, None, False),
        ("send_to_agent", "executing", "Builder", "0", False),
        ("delivery", "awaiting_activity", "Builder", "0", False),
        ("agent_status", "done", "Reviewer", None, True),
        ("summary", "recorded", "Reviewer", "3", True),
        ("speech", "speaking", "Reviewer", "4", False),
    ]):
        records.append(dict(id=str(i), parent=parent, agent_name=name, kind=kind,
            state=state, finished=finished, started_ms=int(time.time()*1000)-7000, updated_ms=int(time.time()*1000)-2000,
            detail={"transcript": "Ask Builder to run the tests"}, transitions=[]))
    return dict(revision=4, voice_state="routing", state="routing", activity=records,
        agents=[dict(agent_name="Builder", status="working"), dict(agent_name="Reviewer", status="done"), dict(agent_name="Helper", status="idle")])


class ActivityTests(unittest.TestCase):
    def test_footer_text_is_visually_centered_on_stream(self):
        for text in ("Orchrd", "Builder", "A"):
            surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, 240, 40)
            ActivityWindow.draw_label(cairo.Context(surface), 120, 8, text, centered=True)
            surface.flush()
            data = bytes(surface.get_data())
            stride = surface.get_stride()
            columns = [x for x in range(240) if any(any(data[y*stride+x*4:y*stride+x*4+4]) for y in range(40))]
            self.assertTrue(columns)
            self.assertAlmostEqual((min(columns) + max(columns)) / 2, 120, delta=3)

    def test_every_action_has_a_distinct_vector_glyph(self):
        self.assertEqual(set(ACTION_GLYPHS), {"utterance", "list_agents", "read_agent", "send_to_agent", "delivery", "respond_to_agent", "cancel_agent", "summary", "speech", "agent_status", "monitor", "latest_result"})
        images = []
        for kind in [*ACTION_GLYPHS, "future_action"]:
            surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, 32, 32)
            draw_glyph(cairo.Context(surface), kind, 16, 16)
            surface.flush()
            data = bytes(surface.get_data())
            self.assertTrue(any(data))
            images.append(data)
        self.assertEqual(len(set(images)), len(images))
        self.assertEqual(action_title("monitor"), "Toad monitoring")

    def test_pane_titles_do_not_merge_or_rename_lane_identities(self):
        snap = dict(agents=[dict(agent_name="w1:p1", display_name="Builder"), dict(agent_name="w2:p1", display_name="Reviewer")])
        self.assertEqual(lane_label(snap, "w1:p1"), "Builder")
        self.assertEqual(lane_label(snap, None), "Orchrd")
        snap["agents"][1]["display_name"] = "Builder"
        self.assertEqual(lane_names(snap), [None, "w1:p1", "w2:p1"])
        self.assertEqual(lane_label(snap, "w2:p1"), "Builder (w2:p1)")
        snap["agents"][0]["display_name"] = "Renamed"
        self.assertEqual(lane_label(snap, "w1:p1"), "Renamed")
        self.assertEqual(lane_label(snap, "missing"), "missing")
    def test_dbus_snapshot_signal_and_service_disappearance(self):
        connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        interface = Gio.DBusNodeInfo.new_for_xml(f'<node><interface name="{BUS}"><method name="GetSnapshot"><arg type="s" direction="out"/></method><signal name="StateChanged"><arg type="s"/></signal></interface></node>').interfaces[0]
        def method(_connection, _sender, _path, _interface, _name, _parameters, invocation):
            invocation.return_value(GLib.Variant("(s)", (json.dumps(fixture()),)))
        registration = connection.register_object(PATH, interface, method, None, None)
        owner = Gio.bus_own_name_on_connection(connection, BUS, Gio.BusNameOwnerFlags.NONE, None, None)
        app = Gtk.Application(application_id="io.github.kacper.Orchrd.BusTest", flags=Gio.ApplicationFlags.NON_UNIQUE)
        app.register(None)
        window = ActivityWindow(app)
        def until(predicate):
            deadline = time.monotonic() + 3
            context = GLib.MainContext.default()
            while not predicate() and time.monotonic() < deadline:
                while context.pending():
                    context.iteration(False)
                time.sleep(0.005)
            self.assertTrue(predicate())
        try:
            until(lambda: window.snapshot.get("revision") == 4)
            snap = fixture()
            snap["revision"] = 5
            snap["agents"][0]["status"] = "blocked"
            connection.emit_signal(None, PATH, BUS, "StateChanged", GLib.Variant("(s)", (json.dumps(snap),)))
            until(lambda: window.snapshot.get("revision") == 5)
            self.assertEqual(window.snapshot["agents"][0]["status"], "blocked")
            Gio.bus_unown_name(owner)
            owner = None
            until(lambda: not window.connected)
            self.assertEqual(window.snapshot, {})
        finally:
            window.close()
            window.destroy()
            if owner is not None:
                Gio.bus_unown_name(owner)
            connection.unregister_object(registration)

    def test_lane_mapping_uses_explicit_agent_and_kind(self):
        snap = fixture()
        self.assertEqual(lane_names(snap), [None, "Builder", "Reviewer", "Helper"])
        self.assertEqual(lane_for(snap["activity"][1], lane_names(snap)), "Builder")
        self.assertIsNone(lane_for(snap["activity"][4], lane_names(snap)))

    def test_invalid_target_does_not_create_a_lane(self):
        snap = fixture()
        invalid = dict(snap["activity"][1], id="invalid", agent_name="orchid", state="failed")
        snap["activity"].append(invalid)
        self.assertNotIn("orchid", lane_names(snap))
        self.assertIsNone(lane_for(invalid, lane_names(snap)))
        # Previously observed agents keep their historical lanes after departure.
        observed = dict(invalid, id="observed", agent_name="departed", kind="agent_status")
        snap["activity"].append(observed)
        self.assertIn("departed", lane_names(snap))

    def test_native_render_details_revision_guard_and_close(self):
        app = Gtk.Application(application_id="io.github.kacper.Orchrd.Test", flags=Gio.ApplicationFlags.NON_UNIQUE)
        app.register(None)
        window = ActivityWindow(app)
        window.connected = True
        window.accept(json.dumps(fixture()))
        self.assertIs(window.footer.get_parent(), window.scroll.get_parent())
        self.assertEqual(window.canvas.get_content_width(), 512)
        self.assertEqual(window.footer_labels()[1], (192, "Builder"))
        window.scroll.get_hadjustment().configure(128, 0, 1200, 1, 10, 500)
        self.assertEqual(window.footer_labels()[1], (64, "Builder"))
        window.scroll.get_vadjustment().configure(300, 0, 1200, 1, 10, 500)
        self.assertEqual(window.footer_labels()[1], (64, "Builder"))
        footer_surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, 980, 46)
        window.draw_footer(window.footer, cairo.Context(footer_surface), 980, 46)
        surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, 980, 620)
        with patch.object(window, "draw_status", wraps=window.draw_status) as statuses:
            window.draw(window.canvas, cairo.Context(surface), 980, 620)
            self.assertEqual(statuses.call_count, 10)
            for call, (x, y, _) in zip(statuses.call_args_list, window.points):
                self.assertEqual(call.args[1:3], (x, y))
        with patch.object(window, "draw_label") as label:
            caption_surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, 160, 130)
            window.draw_status(cairo.Context(caption_surface), 64, 75, "done")
            self.assertEqual(label.call_args.args[1:4], (64, 99, "done"))
            self.assertTrue(label.call_args.kwargs["centered"])
        if os.environ.get("ORCHRD_ACTIVITY_PREVIEW"):
            surface.write_to_png(os.environ["ORCHRD_ACTIVITY_PREVIEW"])
        self.assertEqual(len(window.points), 10)
        window.clicked(None, 1, *window.points[1][:2])
        self.assertEqual(window.selected, "1")
        self.assertEqual(window.inspector_title.get_text(), "Send request")
        tooltip = Mock()
        self.assertTrue(window.node_tooltip(window.canvas, *window.points[1][:2], False, tooltip))
        tooltip.set_text.assert_called_with("Send request · executing")
        self.assertTrue(window.node_tooltip(window.canvas, 0, 0, True, tooltip))
        self.assertFalse(window.node_tooltip(window.canvas, 0, 0, False, tooltip))
        buffer = window.detail.get_buffer()
        self.assertIn("Ask Builder", buffer.get_text(buffer.get_start_iter(), buffer.get_end_iter(), False))
        old = fixture()
        old["revision"] = 3
        old["agents"] = []
        window.accept(json.dumps(old))
        self.assertEqual(len(window.snapshot["agents"]), 3)
        window.close()
        window.destroy()
        # Late D-Bus initialization and scheduled scrolling must be harmless.
        context = GLib.MainContext.default()
        while context.pending():
            context.iteration(False)


if __name__ == "__main__":
    unittest.main()
