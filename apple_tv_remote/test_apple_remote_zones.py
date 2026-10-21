import sys
import types
import unittest
from unittest.mock import MagicMock

# apple_remote_mouse imports evdev at module scope; stub it so the tests can run
# without uinput access. The ecodes values only need to be distinct.
ecodes = types.SimpleNamespace(
    KEY_ENTER=28, KEY_1=2, BTN_LEFT=272, BTN_RIGHT=273,
    EV_KEY=1, EV_REL=2, REL_X=0, REL_Y=1, REL_WHEEL=8, REL_HWHEEL=6,
)
sys.modules.setdefault("evdev", types.SimpleNamespace(UInput=MagicMock, ecodes=ecodes))
sys.modules.setdefault("dbus_next", types.SimpleNamespace(BusType=MagicMock))
sys.modules.setdefault("dbus_next.aio", types.SimpleNamespace(MessageBus=MagicMock))

from apple_remote_mouse import TouchMouse  # noqa: E402
from apple_remote_zones import Region, TouchZones, ZoneConfig  # noqa: E402

PAD_SIZE_MM = TouchMouse.PAD_SIZE_MM
CONFIG = ZoneConfig()
# The pad's extremes measured by sweeping a real A1513 pad.
X_MIN, X_MAX = CONFIG.x_min, CONFIG.x_max
Y_MIN, Y_MAX = CONFIG.y_min, CONFIG.y_max
X_PER_MM = (X_MAX - X_MIN) / PAD_SIZE_MM
Y_PER_MM = (Y_MAX - Y_MIN) / PAD_SIZE_MM
# 2 mm inside the 4 mm edge strips.
CENTER = (X_MIN + (X_MAX - X_MIN) // 2, Y_MIN + (Y_MAX - Y_MIN) // 2)
RIGHT = (X_MAX - 2 * X_PER_MM, CENTER[1])
TOP = (CENTER[0], Y_MAX - 2 * Y_PER_MM)
TOP_RIGHT = (X_MAX - 2 * X_PER_MM, Y_MAX - 2 * Y_PER_MM)


class FakeUInput:
    def __init__(self):
        self.events = []

    def write(self, kind, code, value):
        self.events.append((kind, code, value))

    def syn(self):
        self.events.append(("syn",))

    def keys(self, code=None):
        return [event for event in self.events
                if event[0] == ecodes.EV_KEY and (code is None or event[1] == code)]

    def rels(self, code=None):
        return [event for event in self.events
                if event[0] == ecodes.EV_REL and (code is None or event[1] == code)]


class ZoneRegions(unittest.TestCase):
    def setUp(self):
        self.zones = TouchZones(PAD_SIZE_MM, ZoneConfig(edge_mm=4.0))

    def test_center_is_outside_the_strips(self):
        self.assertIs(self.zones.region(*CENTER), Region.CENTER)

    def test_right_strip(self):
        self.assertIs(self.zones.region(*RIGHT), Region.RIGHT)

    def test_top_strip(self):
        self.assertIs(self.zones.region(*TOP), Region.TOP)

    def test_corner_is_the_intersection(self):
        self.assertIs(self.zones.region(*TOP_RIGHT), Region.CORNER)

    def test_strip_width_is_configurable(self):
        zones = TouchZones(PAD_SIZE_MM, ZoneConfig(edge_mm=8.0))
        six_mm_inside = (X_MAX - 6 * X_PER_MM, CENTER[1])
        self.assertIs(zones.region(*six_mm_inside), Region.RIGHT)
        self.assertIs(self.zones.region(*six_mm_inside), Region.CENTER)


class MeasuredPadGeometry(unittest.TestCase):
    """Literal values captured from the real pad; guards the non-zero origin."""

    # Sweeping the pad to its extremes produced these reports.
    LEAN_RIGHT = -182
    LEAN_LEFT = -1818
    LEAN_TOP = 614
    LEAN_BOTTOM = -1074

    def setUp(self):
        self.zones = TouchZones(37.0, ZoneConfig())

    def test_extremes_are_the_strip_edges(self):
        self.assertIs(self.zones.region(self.LEAN_RIGHT, CENTER[1]), Region.RIGHT)
        self.assertIs(self.zones.region(CENTER[0], self.LEAN_TOP), Region.TOP)
        self.assertIs(self.zones.region(self.LEAN_RIGHT, self.LEAN_TOP), Region.CORNER)

    def test_opposite_extremes_are_centre(self):
        self.assertIs(self.zones.region(self.LEAN_LEFT, CENTER[1]), Region.CENTER)
        self.assertIs(self.zones.region(CENTER[0], self.LEAN_BOTTOM), Region.CENTER)

    def test_pad_box_is_not_centred_on_zero(self):
        # A zero-centred model would put the right edge at +818 and leave it
        # unreachable, since the pad never reports past -182.
        self.assertNotEqual(X_MIN + X_MAX, 0)
        self.assertNotEqual(Y_MIN + Y_MAX, 0)
        self.assertAlmostEqual(self.zones.right_edge_mm(X_MAX), 0.0)
        self.assertAlmostEqual(self.zones.right_edge_mm(X_MIN), 37.0)
        self.assertAlmostEqual(self.zones.top_edge_mm(Y_MAX), 0.0)
        self.assertAlmostEqual(self.zones.top_edge_mm(Y_MIN), 37.0)


class ZoneScrolling(unittest.TestCase):
    def setUp(self):
        self.zones = TouchZones(PAD_SIZE_MM, ZoneConfig(scroll_step_mm=2.0))

    def test_right_strip_only_scrolls_vertically(self):
        self.assertEqual(self.zones.scroll(Region.RIGHT, 120.0, 2.0), (0, 1))

    def test_top_strip_only_scrolls_horizontally(self):
        self.assertEqual(self.zones.scroll(Region.TOP, -4.0, 120.0), (2, 0))

    def test_corner_scrolls_both_axes(self):
        self.assertEqual(self.zones.scroll(Region.CORNER, -4.0, -4.0), (2, -2))

    def test_sub_notches_accumulate(self):
        self.assertEqual(self.zones.scroll(Region.RIGHT, 0.0, 1.5), (0, 0))
        self.assertEqual(self.zones.scroll(Region.RIGHT, 0.0, 1.5), (0, 1))

    def test_reset_discards_the_fraction(self):
        self.zones.scroll(Region.RIGHT, 0.0, 1.5)
        self.zones.reset()
        self.assertEqual(self.zones.scroll(Region.RIGHT, 0.0, 1.0), (0, 0))


class ReportBuilder:
    """Build A1513 reports that decode_touches understands."""

    def __init__(self):
        self.tick = 0

    def advance(self, seconds, buttons=0):
        self.tick = (self.tick + round(seconds * 2461)) & 0xFFFF
        return buttons

    def report(self, x=None, y=None, buttons=0, pressure=100):
        finger = bytes(7) if x is None else (
            ((int(x) & 0xFFF) | ((int(y) & 0xFFF) << 12)).to_bytes(3, "little")
            + bytes(2) + bytes([pressure, 0])
        )
        # decode_touches reads the finger record at payload offset 4.
        payload = bytes([0x32, self.tick & 0xFF, self.tick >> 8, 0]) + finger
        return bytes([0x01, buttons]) + payload


class MouseIntegration(unittest.TestCase):
    def setUp(self):
        self.ui = FakeUInput()
        # One millimetre of finger travel per notch keeps the assertions short.
        self.mouse = TouchMouse(self.ui, 1.0, 1.0, edge_mm=4.0, scroll_step_mm=1.0)
        self.builder = ReportBuilder()

    def send(self, seconds, x=None, y=None, buttons=0):
        self.builder.advance(seconds, buttons)
        self.mouse.process(self.builder.report(x, y, buttons))

    def drag(self, start, dx, dy, steps=6):
        x, y = start
        self.send(0.02, x, y)
        for _ in range(steps):
            x, y = x + dx, y + dy
            self.send(0.02, x, y)

    def test_right_edge_drag_scrolls_vertically(self):
        self.drag(RIGHT, 0, 40)
        self.assertNotEqual(self.ui.rels(ecodes.REL_WHEEL), [])
        self.assertEqual(self.ui.rels(ecodes.REL_X), [])
        self.assertEqual(self.ui.rels(ecodes.REL_HWHEEL), [])

    def test_scroll_follows_the_finger(self):
        # Pad coordinates grow toward the top of the remote.
        self.drag(RIGHT, 0, -40)
        self.assertTrue(all(value > 0 for _, _, value in self.ui.rels(ecodes.REL_WHEEL)))
        self.drag(TOP, 40, 0)
        self.assertTrue(all(value < 0 for _, _, value in self.ui.rels(ecodes.REL_HWHEEL)))

    def test_top_edge_drag_scrolls_horizontally(self):
        self.drag(TOP, -40, 0)
        self.assertNotEqual(self.ui.rels(ecodes.REL_HWHEEL), [])
        self.assertEqual(self.ui.rels(ecodes.REL_X), [])
        self.assertEqual(self.ui.rels(ecodes.REL_WHEEL), [])

    def test_center_drag_moves_the_pointer(self):
        self.drag(CENTER, 40, 40)
        self.assertNotEqual(self.ui.rels(ecodes.REL_X), [])
        self.assertEqual(self.ui.rels(ecodes.REL_WHEEL), [])
        self.assertEqual(self.ui.rels(ecodes.REL_HWHEEL), [])

    def test_pressed_edge_drag_still_scrolls(self):
        x, y = RIGHT
        self.send(0.02, x, y, buttons=0x80)
        for _ in range(6):
            y += 40
            self.send(0.02, x, y, buttons=0x80)
        self.assertNotEqual(self.ui.rels(ecodes.REL_WHEEL), [])
        self.assertEqual(self.ui.rels(ecodes.REL_Y), [])

    def test_edge_scroll_does_not_move_the_pointer(self):
        self.drag(RIGHT, 40, 40)
        self.assertEqual(self.ui.rels(ecodes.REL_X), [])
        self.assertEqual(self.ui.rels(ecodes.REL_Y), [])

    def test_edge_contact_keeps_scrolling_after_leaving_the_strip(self):
        x, y = RIGHT
        self.send(0.02, x, y)
        # Drift into the centre; the contact stays latched to the right strip.
        for _ in range(6):
            x -= 2 * X_PER_MM
            y += 40
            self.send(0.02, x, y)
        self.assertNotEqual(self.ui.rels(ecodes.REL_WHEEL), [])
        self.assertEqual(self.ui.rels(ecodes.REL_Y), [])

    def test_top_right_click_is_a_right_click(self):
        self.send(0.02, *TOP_RIGHT)
        self.send(0.02, *TOP_RIGHT, buttons=0x80)
        self.send(0.02, *TOP_RIGHT)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_RIGHT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_LEFT), [])

    def test_center_click_stays_a_left_click(self):
        self.send(0.02, *CENTER)
        self.send(0.02, *CENTER, buttons=0x80)
        self.send(0.02, *CENTER)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_LEFT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_RIGHT), [])

    def test_click_without_a_touch_is_a_left_click(self):
        self.send(0.02, buttons=0x80)
        self.send(0.02)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_LEFT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_RIGHT), [])

    def test_click_matches_the_current_finger_position(self):
        self.send(0.02, *CENTER)
        self.send(0.02, *TOP_RIGHT)
        self.send(0.02, *TOP_RIGHT, buttons=0x80)
        self.send(0.02, *TOP_RIGHT)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_RIGHT)], [1, 0])

    def test_click_and_touch_in_one_report_use_the_corner(self):
        self.send(0.02, *TOP_RIGHT, buttons=0x80)
        self.send(0.02, *TOP_RIGHT)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_RIGHT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_LEFT), [])

    def test_button_only_press_uses_the_latched_corner(self):
        self.send(0.02, *TOP_RIGHT)
        self.send(0.02, buttons=0x80)
        self.send(0.02)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_RIGHT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_LEFT), [])

    def test_corner_click_releases_even_if_region_changed(self):
        self.send(0.02, *TOP_RIGHT)
        self.send(0.02, *TOP_RIGHT, buttons=0x80)
        self.send(0.02, *CENTER, buttons=0x80)
        self.send(0.02, *CENTER)
        self.assertEqual([value for _, _, value in self.ui.keys(ecodes.BTN_RIGHT)], [1, 0])
        self.assertEqual(self.ui.keys(ecodes.BTN_LEFT), [])


if __name__ == "__main__":
    unittest.main()
