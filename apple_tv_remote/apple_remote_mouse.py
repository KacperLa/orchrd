#!/usr/bin/env python3
"""Use a first-generation Apple TV Siri Remote touch surface as a mouse."""

import argparse
import asyncio
import fcntl
import math
import os
import signal
import sys
import time
from dataclasses import dataclass
from pathlib import Path

from dbus_next import BusType
from dbus_next.aio import MessageBus
from evdev import UInput, ecodes as e

from apple_remote_zones import Region, TouchZones, ZoneConfig

APPLE_VENDOR = "0000004C"
APPLE_REMOTE_PRODUCT = "0000026D"
BLUEZ = "org.bluez"
OM = "org.freedesktop.DBus.ObjectManager"
PROPS = "org.freedesktop.DBus.Properties"
GATT_CHAR = "org.bluez.GattCharacteristic1"
REPORT_UUID = "00002a4d-0000-1000-8000-00805f9b34fb"


@dataclass(frozen=True)
class Touch:
    identifier: int
    x: int
    y: int
    pressure: int


def decode_finger(data: bytes) -> Touch:
    """Decode one seven-byte A1513 touch record."""
    if len(data) != 7:
        raise ValueError("an A1513 finger record must contain seven bytes")
    # Two signed 12-bit coordinates share three bytes. The upper nibble
    # of byte 1 belongs to Y, not X. Dropping it quantizes Y by 16x.
    packed = int.from_bytes(data[:3], 'little')
    x = ((packed & 0xFFF) ^ 0x800) - 0x800
    y = (((packed >> 12) & 0xFFF) ^ 0x800) - 0x800
    identifier = 1 - ((data[6] & 0x08) >> 3)
    return Touch(identifier, x, y, data[5])


def decode_touches(report: bytes) -> list[Touch]:
    """Decode the touch records in a multiplexed A1513 input report."""
    if len(report) < 13 or not report[0] & 0x03:
        return []
    payload = report[2:]
    touches = [decode_finger(payload[4:11])]
    if report[0] & 0x02 and len(payload) >= 18:
        touches.append(decode_finger(payload[11:18]))
    return [touch for touch in touches if touch.pressure > 0]


class TouchMouse:
    # A1513 button mask: Play/Pause=0x08, Menu=0x20.
    KEY_BINDINGS = {0x08: e.KEY_ENTER, 0x20: e.KEY_1}
    X_RANGE = 26180.0 / 16.0
    Y_RANGE = 106.0 * 16.0
    PAD_SIZE_MM = 37.0

    def __init__(
        self,
        ui: UInput,
        x_scale: float,
        y_scale: float,
        acceleration: float = 0.0,
        acceleration_threshold: float = 80.0,
        interpolation_seconds: float = 0.0,
        edge_mm: float = 4.0,
        scroll_step_mm: float = 3.0,
    ) -> None:
        self.ui = ui
        self.x_scale = x_scale
        self.y_scale = y_scale
        self.acceleration = acceleration
        self.acceleration_threshold = acceleration_threshold
        self.interpolation_seconds = interpolation_seconds
        self.zones = TouchZones(
            self.PAD_SIZE_MM,
            ZoneConfig(edge_mm=edge_mm, scroll_step_mm=scroll_step_mm),
        )
        # Latched when a contact starts, so a wobbling edge swipe keeps scrolling.
        self.region = Region.CENTER
        self.button_code = e.BTN_LEFT
        self.last_sample = None
        self.device_tick = None
        self.device_time = 0.0
        self.last_arrival = None
        self.segments = []
        self.duplicates = 0
        self.previous_touch = None
        self.previous_timestamp = None
        self.pending_x = 0.0
        self.pending_y = 0.0
        self.output_x_remainder = 0.0
        self.output_y_remainder = 0.0
        self.button_down = False
        self.keys_down = set()
        self.reports = 0
        self.moves = 0

    def process(self, report: bytes, timestamp: float | None = None) -> None:
        if report and report[0] == 0xFC:
            report = report[1:]
        if len(report) < 2:
            return
        if len(report) != 2 and report[0] & ~0x07:
            return
        for mask, key in self.KEY_BINDINGS.items():
            down = bool(report[1] & mask)
            if down != (key in self.keys_down):
                self.ui.write(e.EV_KEY, key, int(down))
                self.ui.syn()
                if down:
                    self.keys_down.add(key)
                else:
                    self.keys_down.discard(key)
        if timestamp is None:
            timestamp = time.monotonic()
        if self.last_arrival is not None and timestamp - self.last_arrival > 1.0:
            # Rebase after sleep/disconnection, including ambiguous timer wraps.
            self.previous_touch = None
            self.device_tick = None
            self.last_sample = None
            self.region = Region.CENTER
            self.zones.reset()
            self.cancel_motion()
        self.last_arrival = timestamp
        self.reports += 1
        # Decode before the button handling so a corner click uses the current finger position.
        touch = self.report_touch(report)
        continuing = (touch is not None and self.previous_touch is not None
                      and touch.identifier == self.previous_touch.identifier)
        if touch is not None and not continuing:
            # Latch the strip for the whole contact: 4 mm is too narrow to track
            # through a swipe, and drifting out of it must not start a drag.
            self.region = self.zones.region(touch.x, touch.y)
            self.zones.reset()
        clicked = bool(report[1] & 0x80)
        if clicked != self.button_down:
            if clicked:
                # Button-only reports carry no coordinates; then the latched
                # contact position decides, otherwise the current finger does.
                corner = (self.zones.region(touch.x, touch.y) is Region.CORNER
                          if touch is not None else self.region is Region.CORNER)
                self.button_code = e.BTN_RIGHT if corner else e.BTN_LEFT
            self.cancel_motion()
            self.ui.write(e.EV_KEY, self.button_code, int(clicked))
            self.button_down = clicked
            self.ui.syn()
        # Button/IMU-only reports do not mean the finger lifted.
        if not report[0] & 3:
            return
        expected = 20 if report[0] & 2 else 13
        if len(report) < expected or report[2] != 0x32:
            return
        sample = report[2:expected]
        if self.last_sample == sample:
            self.duplicates += 1
            return
        self.last_sample = sample
        tick = int.from_bytes(report[3:5], 'little')
        if self.device_tick is not None:
            ticks = (tick - self.device_tick) & 0xFFFF
            if ticks == 0 or ticks > 0x8000:
                return
            self.device_time += ticks / 2461.0
        self.device_tick = tick
        timestamp = self.device_time

        if continuing:
            dx_raw = touch.x - self.previous_touch.x
            dy_raw = touch.y - self.previous_touch.y
            # Finger order changes and a newly placed finger can otherwise cause a jump.
            if abs(dx_raw) < self.X_RANGE / 3 and abs(dy_raw) < self.Y_RANGE / 3:
                dx_mm = dx_raw * self.PAD_SIZE_MM / self.X_RANGE
                dy_mm = dy_raw * self.PAD_SIZE_MM / self.Y_RANGE
                if self.region is not Region.CENTER:
                    self.cancel_motion()
                    # Pad Y grows toward the top; scroll coordinates grow down.
                    horizontal, vertical = self.zones.scroll(self.region, dx_mm, -dy_mm)
                    self.emit_scroll(horizontal, vertical)
                else:
                    elapsed = max(timestamp - self.previous_timestamp, 1e-3)
                    speed = math.hypot(dx_mm, dy_mm) / elapsed
                    # A bounded velocity curve gives quick swipes more reach while
                    # retaining one-to-one precision at low speed.
                    speed_squared = speed * speed
                    threshold_squared = self.acceleration_threshold**2
                    gain = 1.0 + self.acceleration * (
                        speed_squared / (speed_squared + threshold_squared)
                    )
                    dx = dx_raw * self.x_scale * gain
                    # The remote's Y coordinate increases toward its top; REL_Y increases down.
                    dy = -dy_raw * self.y_scale * gain
                    if not dx_raw and not dy_raw:
                        self.cancel_motion(clear_fraction=False)
                    elif self.interpolation_seconds <= 0:
                        self.emit_motion(dx, dy)
                    else:
                        if dx * self.pending_x < 0 or dy * self.pending_y < 0:
                            self.cancel_motion()
                        self.segments.append([time.monotonic(), dx, dy, 0.0])
                        self.pending_x += dx
                        self.pending_y += dy
            else:
                self.cancel_motion()
        else:
            self.cancel_motion()

        if touch is None:
            self.previous_timestamp = None
            self.region = Region.CENTER
            self.zones.reset()
        else:
            self.previous_timestamp = timestamp
        self.previous_touch = touch

    def report_touch(self, report: bytes) -> Touch | None:
        """Return the tracked finger in a touch report without changing state."""
        if not report[0] & 3:
            return None
        expected = 20 if report[0] & 2 else 13
        if len(report) < expected or report[2] != 0x32:
            return None
        touches = decode_touches(report)
        if not touches:
            return None
        if self.previous_touch is not None:
            return next(
                (item for item in touches if item.identifier == self.previous_touch.identifier),
                touches[0],
            )
        return touches[0]

    def cancel_motion(self, clear_fraction: bool = True) -> None:
        self.segments.clear()
        self.pending_x = self.pending_y = 0.0
        if clear_fraction:
            self.output_x_remainder = self.output_y_remainder = 0.0

    def flush_motion(self, now: float) -> None:
        """Each optional segment has its own deadline; new data cannot defer it."""
        keep = []
        dx = dy = 0.0
        for start, x, y, previous in self.segments:
            fraction = min(1.0, max(0.0, (now - start) / self.interpolation_seconds))
            dx += x * (fraction - previous)
            dy += y * (fraction - previous)
            if fraction < 1.0:
                keep.append([start, x, y, fraction])
        self.segments = keep
        self.pending_x = sum(x * (1 - f) for _, x, _, f in keep)
        self.pending_y = sum(y * (1 - f) for _, _, y, f in keep)
        self.emit_motion(dx, dy)

    def emit_scroll(self, horizontal: int, vertical: int) -> None:
        if horizontal:
            self.ui.write(e.EV_REL, e.REL_HWHEEL, horizontal)
        if vertical:
            self.ui.write(e.EV_REL, e.REL_WHEEL, vertical)
        if horizontal or vertical:
            self.ui.syn()

    def emit_motion(self, dx: float, dy: float) -> None:
        self.output_x_remainder += dx
        self.output_y_remainder += dy
        dx, dy = int(self.output_x_remainder), int(self.output_y_remainder)
        self.output_x_remainder -= dx
        self.output_y_remainder -= dy
        if dx or dy:
            self.ui.write(e.EV_REL, e.REL_X, dx)
            self.ui.write(e.EV_REL, e.REL_Y, dy)
            self.ui.syn()
            self.moves += 1

    def release(self) -> None:
        self.cancel_motion()
        self.zones.reset()
        self.region = Region.CENTER
        for key in self.keys_down:
            self.ui.write(e.EV_KEY, key, 0)
        if self.keys_down:
            self.ui.syn()
        self.keys_down.clear()
        if self.button_down:
            self.ui.write(e.EV_KEY, self.button_code, 0)
            self.ui.syn()
            self.button_down = False


def find_bluetooth_hidraw(mac: str) -> str:
    """Locate the paired remote's Bluetooth hidraw device."""
    wanted_mac = mac.lower()
    for uevent in Path("/sys/class/hidraw").glob("hidraw*/device/uevent"):
        values = {}
        for line in uevent.read_text().splitlines():
            key, _, value = line.partition("=")
            values[key] = value
        if (
            values.get("HID_ID") == f"0005:{APPLE_VENDOR}:{APPLE_REMOTE_PRODUCT}"
            and values.get("HID_UNIQ", "").lower() == wanted_mac
        ):
            return f"/dev/{uevent.parts[-3]}"
    raise RuntimeError(
        "Apple remote Bluetooth HID endpoint not found; wake the remote and reconnect it"
    )


def hid_set_feature(fd: int, report: bytes) -> None:
    """Send a HID feature report through hidraw."""
    # _IOWR('H', 0x06, len), from linux/hidraw.h HIDIOCSFEATURE.
    ioctl = (3 << 30) | (len(report) << 16) | (ord("H") << 8) | 0x06
    fcntl.ioctl(fd, ioctl, bytearray(report), True)


async def get_input_report(bus: MessageBus, mac: str):
    """Return the notifying HID report characteristic and its properties."""
    device_path = f"/org/bluez/hci0/dev_{mac.upper().replace(':', '_')}"
    intro = await bus.introspect(BLUEZ, "/")
    manager = bus.get_proxy_object(BLUEZ, "/", intro).get_interface(OM)
    objects = await manager.call_get_managed_objects()
    for path, interfaces in objects.items():
        characteristic = interfaces.get(GATT_CHAR)
        if not path.startswith(device_path + "/") or not characteristic:
            continue
        if characteristic["UUID"].value.lower() != REPORT_UUID:
            continue
        if "notify" not in characteristic["Flags"].value:
            continue
        char_intro = await bus.introspect(BLUEZ, path)
        obj = bus.get_proxy_object(BLUEZ, path, char_intro)
        return obj.get_interface(GATT_CHAR), obj.get_interface(PROPS)
    raise RuntimeError("the A1513 input report characteristic was not found")


async def run(args: argparse.Namespace) -> None:
    x_scale = (
        args.sensitivity * args.pointer_speed * TouchMouse.PAD_SIZE_MM / TouchMouse.X_RANGE
    )
    y_scale = (
        args.sensitivity * args.pointer_speed * TouchMouse.PAD_SIZE_MM / TouchMouse.Y_RANGE
    )
    capabilities = {
        e.EV_KEY: [e.BTN_LEFT, e.BTN_RIGHT, *TouchMouse.KEY_BINDINGS.values()],
        e.EV_REL: [e.REL_X, e.REL_Y, e.REL_WHEEL, e.REL_HWHEEL],
    }

    with UInput(capabilities, name="Apple TV Remote Touch Mouse") as ui:
        mouse = TouchMouse(
            ui,
            x_scale,
            y_scale,
            args.acceleration,
            args.acceleration_threshold,
            args.interpolation_ms / 1000.0,
            args.edge_mm,
            args.scroll_step_mm,
        )
        bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
        loop = asyncio.get_running_loop()
        stop = asyncio.Event()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, stop.set)

        async def output_motion() -> None:
            interval = 1.0 / args.output_rate
            deadline = loop.time()
            while not stop.is_set():
                deadline += interval
                await asyncio.sleep(max(0.0, deadline - loop.time()))
                now = loop.time()
                mouse.flush_motion(now)
                if now > deadline + interval:
                    deadline = now

        output_task = asyncio.create_task(output_motion()) if args.interpolation_ms else None
        try:
            device_path = f"/org/bluez/hci0/dev_{args.mac.upper().replace(':', '_')}"
            while not stop.is_set():
                fd = None
                input_report = properties = changed = None
                try:
                    # Connect is harmless when already connected and is necessary
                    # after this remote's aggressive idle disconnects.
                    intro = await bus.introspect(BLUEZ, device_path)
                    remote = bus.get_proxy_object(BLUEZ, device_path, intro)
                    try:
                        await remote.get_interface("org.bluez.Device1").call_connect()
                    except Exception:
                        pass

                    # GATT objects and hidraw nodes are rebuilt asynchronously.
                    deadline = loop.time() + 8.0
                    while True:
                        try:
                            input_report, properties = await get_input_report(bus, args.mac)
                            device = args.hidraw or find_bluetooth_hidraw(args.mac)
                            fd = os.open(device, os.O_RDWR | os.O_NONBLOCK)
                            break
                        except (FileNotFoundError, PermissionError, RuntimeError):
                            if loop.time() >= deadline:
                                raise
                            await asyncio.sleep(0.2)

                    def changed(interface: str, changes: dict, invalidated: list[str]) -> None:
                        value = changes.get("Value")
                        if interface == GATT_CHAR and value is not None:
                            mouse.process(bytes(value.value))

                    properties.on_properties_changed(changed)
                    await input_report.call_start_notify()
                    hid_set_feature(fd, b"\xF0\xAF")
                    print(
                        f"Apple remote relative touch mouse active on {device}; "
                        f"12-bit coordinates, interpolation={args.interpolation_ms:g} ms.",
                        file=sys.stderr,
                        flush=True,
                    )
                    while not stop.is_set() and Path(device).exists():
                        await asyncio.sleep(0.25)
                    if not stop.is_set():
                        print("Apple remote disconnected; reconnecting...", file=sys.stderr, flush=True)
                except Exception as error:
                    if not stop.is_set():
                        print(f"Apple remote unavailable ({error}); retrying...",
                              file=sys.stderr, flush=True)
                finally:
                    mouse.release()
                    if properties is not None and changed is not None:
                        try:
                            properties.off_properties_changed(changed)
                        except Exception:
                            pass
                    if fd is not None:
                        os.close(fd)
                    if input_report is not None:
                        try:
                            await input_report.call_stop_notify()
                        except Exception:
                            pass
                if not stop.is_set():
                    await asyncio.sleep(0.5)
        finally:
            if output_task:
                output_task.cancel()
                try:
                    await output_task
                except asyncio.CancelledError:
                    pass
            mouse.release()
            bus.disconnect()
        print(
            f"Stopped after {mouse.reports} reports, {mouse.duplicates} duplicates, "
            f"and {mouse.moves} pointer updates.",
            file=sys.stderr,
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mac", default="28:EC:95:12:B9:E5")
    parser.add_argument("--hidraw", help="override the automatically detected hidraw device")
    parser.add_argument(
        "--sensitivity",
        type=float,
        default=1.0,
        help="relative pointer speed multiplier (default: 1.0)",
    )
    parser.add_argument(
        "--pointer-speed",
        type=float,
        default=3.0,
        metavar="PIXELS_PER_MM",
        help="base pointer travel per millimeter of finger motion (default: 3)",
    )
    parser.add_argument(
        "--acceleration",
        type=float,
        default=3.0,
        help="maximum additional gain for fast swipes (default: 3)",
    )
    parser.add_argument(
        "--acceleration-threshold",
        type=float,
        default=80.0,
        metavar="MM_PER_SECOND",
        help="speed where half the acceleration is applied (default: 80)",
    )
    parser.add_argument(
        "--interpolation-ms",
        "--smoothing-ms",
        dest="interpolation_ms",
        type=float,
        default=6.0,
        help="independent linear segments in ms; zero disables interpolation (default: 6)",
    )
    parser.add_argument(
        "--edge-mm",
        type=float,
        default=4.0,
        metavar="MILLIMETERS",
        help="width of the right/top scroll strips (default: 4)",
    )
    parser.add_argument(
        "--scroll-step-mm",
        type=float,
        default=3.0,
        metavar="MILLIMETERS",
        help="finger travel per scroll notch in the edge strips (default: 3)",
    )
    parser.add_argument(
        "--output-rate",
        type=float,
        default=240.0,
        metavar="HZ",
        help="virtual mouse update rate (default: 240)",
    )
    args = parser.parse_args()
    if args.sensitivity <= 0:
        parser.error("--sensitivity must be greater than zero")
    if args.pointer_speed <= 0:
        parser.error("--pointer-speed must be greater than zero")
    if args.acceleration < 0:
        parser.error("--acceleration cannot be negative")
    if args.acceleration_threshold <= 0:
        parser.error("--acceleration-threshold must be greater than zero")
    if args.interpolation_ms < 0:
        parser.error("--interpolation-ms cannot be negative")
    if args.edge_mm <= 0:
        parser.error("--edge-mm must be greater than zero")
    if args.scroll_step_mm <= 0:
        parser.error("--scroll-step-mm must be greater than zero")
    if args.output_rate <= 0:
        parser.error("--output-rate must be greater than zero")
    # Prevent competing instances after a terminal/tool session is interrupted.
    runtime = Path(os.environ.get('XDG_RUNTIME_DIR', '/tmp'))
    lock_path = runtime / f'apple-touch-mouse-{os.getuid()}.lock'
    lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.exit(1, 'An Apple touch mouse instance is already running.\n')
        asyncio.run(run(args))
    finally:
        os.close(lock_fd)


if __name__ == "__main__":
    main()
