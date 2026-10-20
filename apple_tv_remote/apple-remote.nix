{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.apple-tv-remote;
  account = config.users.users.${cfg.user};
  runtimeDir = "/run/user/${toString account.uid}";
  # The mouse script imports sibling modules, so the directory is one store path.
  source = builtins.path {
    path = ./.;
    name = "apple-tv-remote";
    filter = path: type: baseNameOf path != "__pycache__";
  };
  macArgument = lib.escapeShellArgs [
    "--mac"
    cfg.macAddress
  ];
  python = pkgs.python3.withPackages (ps: [
    ps.dbus-next
    ps.evdev
  ]);
  common = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "bluetooth.service" ];
    after = [ "bluetooth.service" ];
    unitConfig.StartLimitIntervalSec = 0;
    path = [
      pkgs.bluez
      pkgs.pipewire
      pkgs.wireplumber
    ];
    environment = {
      XDG_RUNTIME_DIR = runtimeDir;
      DBUS_SESSION_BUS_ADDRESS = "unix:path=${runtimeDir}/bus";
    };
  };
  protection = {
    User = cfg.user;
    Group = account.group;
    Restart = "always";
    RestartSec = 3;
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = "read-only";
    ReadWritePaths = [ "-${runtimeDir}" ];
    TimeoutStopSec = 15;
  };
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
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          builtins.hasAttr cfg.user config.users.users && config.users.users.${cfg.user}.uid != null;
        message = "services.apple-tv-remote.user must name an existing user with users.users.<name>.uid set.";
      }
    ];
    hardware.bluetooth.enable = true;
    hardware.uinput.enable = true;
    services.udev.extraRules = ''
      SUBSYSTEM=="hidraw", KERNELS=="0005:004C:026D.*", OWNER="${cfg.user}", MODE="0600"
    '';
    systemd.services.apple-remote-mouse = common // {
      description = "Apple remote touchpad and automatic Bluetooth connection";
      serviceConfig = protection // {
        SupplementaryGroups = [ "uinput" ];
        ExecStart = "${python}/bin/python ${source}/apple_remote_mouse.py ${macArgument}";
      };
    };
    systemd.services.apple-remote-mic = common // {
      description = "Apple remote microphone and Orchard voice control";
      wants = common.wants ++ [ "apple-remote-mouse.service" ];
      after = common.after ++ [ "apple-remote-mouse.service" ];
      environment = common.environment // {
        APPLE_OPUS_LIBRARY = "${pkgs.libopus}/lib/libopus.so.0";
        APPLE_REMOTE_TUNE_LINK = "1";
      };
      serviceConfig = protection // {
        # Use the installed orchrdctl to control the user Orchard service.
        Environment = "PATH=${account.home}/.nix-profile/bin:/etc/profiles/per-user/${cfg.user}/bin:/run/current-system/sw/bin:${pkgs.bluez}/bin:${pkgs.pipewire}/bin:${pkgs.wireplumber}/bin";
        AmbientCapabilities = [
          "CAP_NET_RAW"
          "CAP_NET_ADMIN"
        ];
        CapabilityBoundingSet = [
          "CAP_NET_RAW"
          "CAP_NET_ADMIN"
        ];
        ExecStart = "${python}/bin/python ${source}/apple_remote_mic.py --direct-monitor ${macArgument}";
      };
    };
  };
}
