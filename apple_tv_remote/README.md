# Apple TV Remote

A Rust daemon that turns a first-generation Apple TV Siri Remote (A1513) into a
Bluetooth touchpad, button source and Opus microphone on Linux. The touchpad is
published through uinput, and the microphone as a PipeWire source.

This subproject is standalone: it has its own flake, its own lock file and no
dependency on the rest of the repository. It is intended to move to its own
repository. Holding Siri captures microphone audio into the published source;
any PipeWire consumer can record from it.

## Architecture

One process replaces the previous pair of Python daemons:

- A single `AF_BLUETOOTH`/`BTPROTO_HCI` monitor socket delivers every packet
  from the controller. Touchpad, button and microphone reports all arrive on the
  same ATT notification characteristic, so there is one capture path and one
  parser.
- Opus packets are decoded in-process with `libopus` on the capture thread.
  Decoded PCM goes into a wait-free `rtrb` ring buffer that the PipeWire process
  callback reads from; the previous implementation piped raw PCM through a
  `pw-cat` child process and a Python `bytearray` queue.
- No process is spawned and nothing is allocated on the PipeWire realtime path.
  PipeWire itself drives the playout cadence, so the daemon has no audio timer
  and no periodic wakeup between buffers.
- Pointer interpolation has no timer of its own either: the packet loop's poll
  doubles as the tick, shortened to the output rate only while travel from a
  recent report is still queued.

| Module | Responsibility |
| --- | --- |
| `layout.rs` | Scroll strips, click-corner geometry and pointer interpolation |
| `touch.rs` | A1513 report decoding and the pointer/scroll/click state machine |
| `report.rs` | ATT reassembly, HID report parsing, HCI event parsing |
| `audio.rs` | Opus decode, jitter buffering and playout |
| `pipewire.rs` | The published microphone source |
| `bluetooth.rs` | HCI monitor socket, connection handle lookup, hidraw access |
| `gatt.rs` | BlueZ notification subscription and report-rate unlock |
| `mouse.rs` | uinput sink for the touch state machine |
| `inspect.rs` | Offline decoding of a btsnoop capture |

## Behaviour

Dragging the touchpad's right edge scrolls vertically, dragging its top edge
scrolling horizontally, and a mechanical click in the top-right corner is a
right click. The strips are measured inward from the pad's measured extreme, so
only a finger rolled against the remote's own side scrolls. A contact that
starts in a strip keeps scrolling for the whole touch.

Pointer motion is interpolated: each report's travel is spread over
`--interpolation-ms` (default 6, alias `--smoothing-ms`) and emitted at
`--output-rate` ticks per second (default 240), so the pointer follows a smooth
path instead of stepping once per report. `--interpolation-ms 0` emits each
report whole. Travel still queued when the finger reverses, pauses, clicks or
lifts is dropped rather than finished, so one gesture never moves the pointer
into the next one.

Holding Siri starts capturing microphone audio into the published source.
Releasing waits for late Bluetooth packets and the final PipeWire buffers to
deliver the tail — up to ten seconds — before the capture session is over.
Decoded speech is never dropped to make room: a consumer that attaches late
hears the whole utterance, and speech older than the two-minute ring bound
stops the daemon instead of silently truncating a recording.

Only one daemon may run per user: a second instance would publish a duplicate
uinput device and microphone node, so the daemon holds an `flock` on
`$XDG_RUNTIME_DIR/apple-tv-remote-<uid>.lock` and exits if it is taken.

## Development

The flake provides everything the build needs: the PipeWire headers (the
bindings are generated with bindgen, which needs libclang), libopus, and a
private audio session for the integration test.

```bash
cd apple_tv_remote
nix develop
cargo test
dbus-run-session -- cargo test --test pipewire_session
```

`tests/pipewire_session.rs` starts its own `pipewire` and `wireplumber` in a
temporary runtime directory, so it publishes no nodes into your desktop session.
It is skipped when those binaries are not on `PATH`. `nix build` excludes it by
test name through `cargoTestFlags`, since the sandbox cannot start a server.

## Diagnostics

`--inspect-capture FILE` decodes a `btmon`/btsnoop capture offline and prints
frame counts, computed duration, sequence gaps, concealment frames and peak
level as JSON.

## NixOS module

```nix
inputs.apple-tv-remote = {
  url = "github:KacperLa/orchrd?dir=apple_tv_remote";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

```nix
{
  imports = [ apple-tv-remote.nixosModules.default ];
  users.users.you.uid = 1000; # Use your existing user's numeric UID.

  services.apple-tv-remote = {
    enable = true;
    user = "you";
    macAddress = "AA:BB:CC:DD:EE:FF";
    edgeMm = 2; # Scroll strip width; raise it if edge scrolling feels unreachable.
    cornerMm = 4; # Top-right right-click zone; must be at least edgeMm.
  };
}
```

Pair the remote first. The module creates the system service
`apple-tv-remote.service`, which runs as the selected user with `CAP_NET_RAW`
(monitor socket) and `CAP_NET_ADMIN` (LE connection update). It needs the
selected user's PipeWire session.
