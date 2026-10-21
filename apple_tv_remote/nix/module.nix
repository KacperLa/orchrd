# NixOS module: the Rust Apple TV remote connection daemon.
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
  macArgument = lib.escapeShellArgs [
    "--mac"
    cfg.macAddress
  ];
in
{
  options.services.apple-tv-remote = {
    enable = lib.mkEnableOption "the Apple TV remote Bluetooth connection";
    user = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "alice";
      description = "Existing user the daemon and its hidraw access belong to";
    };
    macAddress = lib.mkOption {
      type = lib.types.strMatching "([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}";
      example = "AA:BB:CC:DD:EE:FF";
      description = "Bluetooth address of the paired Apple TV remote";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.hasAttr cfg.user config.users.users;
        message = "services.apple-tv-remote.user must name an existing user.";
      }
    ];
    hardware.bluetooth.enable = true;
    services.udev.extraRules = ''
      SUBSYSTEM=="hidraw", KERNELS=="0005:004C:026D.*", OWNER="${cfg.user}", MODE="0600"
    '';
    systemd.services.apple-tv-remote = {
      description = "Apple TV remote Bluetooth connection";
      wantedBy = [ "multi-user.target" ];
      wants = [ "bluetooth.service" ];
      after = [ "bluetooth.service" ];
      unitConfig.StartLimitIntervalSec = 0;
      environment.RUST_LOG = "apple_tv_remote=info";
      serviceConfig = {
        User = cfg.user;
        Group = config.users.users.${cfg.user}.group;
        Restart = "always";
        RestartSec = 3;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        TimeoutStopSec = 15;
        # A monitor socket needs CAP_NET_RAW.
        AmbientCapabilities = [ "CAP_NET_RAW" ];
        CapabilityBoundingSet = [ "CAP_NET_RAW" ];
        ExecStart = ''
          ${remote}/bin/apple-tv-remote \
            ${macArgument}
        '';
      };
    };
  };
}
