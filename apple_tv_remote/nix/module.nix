# NixOS module: the Rust Apple TV remote daemon.
#
# The daemon binary comes from this flake's own packages, so a host never has
# to pass a package explicitly.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.apple-tv-remote;
  remote = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  account = config.users.users.${cfg.user};
  runtimeDir = "/run/user/${toString account.uid}";
  macArgument = lib.escapeShellArgs [
    "--mac"
    cfg.macAddress
  ];
  pointerArguments = lib.escapeShellArgs [
    "--edge-mm"
    (toString cfg.edgeMm)
    "--corner-mm"
    (toString cfg.cornerMm)
    "--sensitivity"
    (toString cfg.sensitivity)
    "--pointer-speed"
    (toString cfg.pointerSpeed)
  ];
  # Orchrd is optional: the daemon still publishes the microphone source and
  # provides the touchpad without it.
  orchrdArgument = if cfg.orchrd then "--orchrd true" else "--orchrd false";
  path = [
    pkgs.bluez
    pkgs.pipewire
    pkgs.wireplumber
  ];
in
{
  options.services.apple-tv-remote = {
    enable = lib.mkEnableOption "Apple TV remote touchpad and microphone input";
    user = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "alice";
      description = "Existing desktop user with an explicitly assigned numeric UID";
    };
    macAddress = lib.mkOption {
      type = lib.types.strMatching "([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}";
      example = "AA:BB:CC:DD:EE:FF";
      description = "Bluetooth address of the paired Apple TV remote";
    };
    edgeMm = lib.mkOption {
      type = lib.types.ints.positive;
      default = 2;
      example = 3;
      description = ''
        Width of the right and top scroll strips, measured inward from the pad's
        outer extreme. Narrow strips only scroll for a finger pressed against the
        remote's side; wider ones catch pointer movement as scrolling.
      '';
    };
    cornerMm = lib.mkOption {
      type = lib.types.ints.positive;
      default = 4;
      example = 5;
      description = ''
        Side of the top-right corner zone where clicking is a right click. Must be
        at least edgeMm so the click target reaches past the scroll strips.
      '';
    };
    sensitivity = lib.mkOption {
      type = lib.types.float;
      default = 1.0;
      description = "Relative pointer sensitivity multiplier";
    };
    pointerSpeed = lib.mkOption {
      type = lib.types.float;
      default = 3.0;
      description = "Base pointer travel in pixels per millimetre of finger motion";
    };
    orchrd = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Send Siri push-to-talk speech to Orchrd. The daemon drives the Orchrd
        session service over the selected user's session bus, so nothing needs
        to be on the unit's path.
      '';
    };
    tuneLink = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Request a 7.5-15 ms Bluetooth connection interval whenever the remote
        connects. Needs CAP_NET_ADMIN, which the service already holds.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          builtins.hasAttr cfg.user config.users.users && config.users.users.${cfg.user}.uid != null;
        message = "services.apple-tv-remote.user must name an existing user with users.users.<name>.uid set.";
      }
      {
        assertion = cfg.cornerMm >= cfg.edgeMm;
        message = "services.apple-tv-remote.cornerMm must be at least edgeMm.";
      }
    ];
    hardware.bluetooth.enable = true;
    hardware.uinput.enable = true;
    services.udev.extraRules = ''
      SUBSYSTEM=="hidraw", KERNELS=="0005:004C:026D.*", OWNER="${cfg.user}", MODE="0600"
    '';
    systemd.services.apple-tv-remote = {
      description = "Apple TV remote touchpad, buttons and microphone";
      wantedBy = [ "multi-user.target" ];
      wants = [
        "bluetooth.service"
        "pipewire.service"
      ];
      after = [
        "bluetooth.service"
        "graphical-session.target"
      ];
      unitConfig.StartLimitIntervalSec = 0;
      inherit path;
      environment = {
        XDG_RUNTIME_DIR = runtimeDir;
        DBUS_SESSION_BUS_ADDRESS = "unix:path=${runtimeDir}/bus";
        RUST_LOG = "apple_tv_remote=info";
      };
      serviceConfig = {
        User = cfg.user;
        Group = account.group;
        Restart = "always";
        RestartSec = 3;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ReadWritePaths = [ "-${runtimeDir}" ];
        TimeoutStopSec = 15;
        SupplementaryGroups = [ "uinput" ];
        # A monitor socket needs CAP_NET_RAW; the LE connection update is an
        # HCI command, which needs CAP_NET_ADMIN on a raw socket.
        AmbientCapabilities = [
          "CAP_NET_RAW"
          "CAP_NET_ADMIN"
        ];
        CapabilityBoundingSet = [
          "CAP_NET_RAW"
          "CAP_NET_ADMIN"
        ];
        ExecStart = ''
          ${remote}/bin/apple-tv-remote \
            ${macArgument} \
            ${pointerArguments} \
            ${orchrdArgument} \
            --tune-link ${if cfg.tuneLink then "true" else "false"}
        '';
      };
    };
  };
}
