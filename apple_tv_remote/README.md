# Apple TV Remote

A Rust daemon that turns a first-generation Apple TV Siri Remote (A1513) into a
Bluetooth touchpad and button source on Linux, published through uinput.

This subproject is independent of the rest of Orchrd: it has its own flake, its
own lock file and no dependency on the Orchrd crate. It is intended to move to
its own repository.

## Architecture

One process replaces the previous Python mouse daemon:

- A single `AF_BLUETOOTH`/`BTPROTO_HCI` monitor socket delivers every packet
  from the controller. Touchpad and button reports arrive on the same ATT
  notification characteristic, so there is one capture path and one parser.
- Pointer interpolation has no timer of its own: the packet loop's poll doubles
  as the tick, shortened to the output rate only while travel from a recent
  report is still queued.

| Module | Responsibility |
| --- | --- |
| `layout.rs` | Scroll strips, click-corner geometry and pointer interpolation |
| `touch.rs` | A1513 report decoding and the pointer/scroll/click state machine |
| `report.rs` | ATT reassembly, HID report parsing, HCI event parsing |
| `bluetooth.rs` | HCI monitor socket, connection handle lookup, hidraw access |
| `gatt.rs` | BlueZ notification subscription and report-rate unlock |
| `mouse.rs` | uinput sink for the touch state machine |
| `lock.rs` | Single-instance guard |

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

Only one daemon may run per user: a second instance would publish a duplicate
uinput device, so the daemon holds an `flock` on
`$XDG_RUNTIME_DIR/apple-tv-remote-<uid>.lock` and exits if it is taken.

## Development

The flake provides everything the build and test suite needs:

```bash
cd apple_tv_remote
nix develop
cargo test
```

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
(monitor socket) and `CAP_NET_ADMIN` (LE connection update) plus uinput group
access for the touchpad device.
