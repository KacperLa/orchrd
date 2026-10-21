# Apple TV Remote

A small Rust daemon that establishes the Bluetooth connection to a
first-generation Apple TV Siri Remote (A1513) and keeps it reporting.

The remote's HID input reports (touchpad, buttons, microphone) all arrive
over one GATT characteristic. The daemon asks BlueZ to connect and subscribe
to that characteristic, and sends the HID feature report through hidraw that
unlocks the remote's full report rate. It keeps the subscription alive across
the remote's idle disconnects and redoes the setup on every reconnection.
It currently interprets no reports; touchpad, button and microphone handling
may be added later.

This subproject is independent of the rest of Orchrd: it has its own flake,
its own lock file and no dependency on the Orchrd crate. It is intended to
move to its own repository.

## Architecture

- A single `AF_BLUETOOTH`/`BTPROTO_HCI` monitor socket (CAP_NET_RAW) keeps the
  process running while reports flow; it is also the transport later input
  handling will read from.
- `gatt.rs` runs on its own thread: BlueZ connection, notification
  subscription and the report-rate unlock, retried until the remote accepts
  them and again after every reconnect. The system-bus connection is held for
  as long as the remote stays connected, because BlueZ releases a
  `StartNotify` subscription when the connection that acquired it goes away.
- `bluetooth.rs` holds the monitor socket, the address parser and the hidraw
  feature-report unlock.

| Module | Responsibility |
| --- | --- |
| `gatt.rs` | BlueZ connection, notification subscription and report-rate unlock |
| `bluetooth.rs` | HCI monitor socket and hidraw access |
| `report.rs` | HCI monitor record framing |

## Development

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
  users.users.you.uid = 1000; # Use your existing user's numeric UID.

  services.apple-tv-remote = {
    enable = true;
    user = "you";
    macAddress = "AA:BB:CC:DD:EE:FF";
  };
}
```

Pair the remote first. The module creates the system service
`apple-tv-remote.service`, which runs as the selected user with `CAP_NET_RAW`
for the monitor socket. Logs use system
`journalctl -u apple-tv-remote.service`.
