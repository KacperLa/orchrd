{ pkgs, ... }:
let
  python = pkgs.python3.withPackages (ps: [ ps.dbus-next ps.evdev ]);
  common = {
    wantedBy = [ "multi-user.target" ];
    wants = [ "bluetooth.service" ];
    after = [ "bluetooth.service" ];
    unitConfig.StartLimitIntervalSec = 0;
    path = [ pkgs.bluez pkgs.pipewire pkgs.wireplumber ];
    environment = {
      XDG_RUNTIME_DIR = "/run/user/1000";
      DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus";
    };
  };
  protection = {
    User = "kacper";
    Group = "users";
    Restart = "always";
    RestartSec = 3;
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = "read-only";
    ReadWritePaths = [ "-/run/user/1000" ];
    TimeoutStopSec = 15;
  };
in {
  hardware.bluetooth.enable = true;
  hardware.uinput.enable = true;
  services.udev.extraRules = ''
    SUBSYSTEM=="hidraw", KERNELS=="0005:004C:026D.*", OWNER="kacper", MODE="0600"
  '';
  systemd.services.apple-remote-mouse = common // {
    description = "Apple remote touchpad and automatic Bluetooth connection";
    serviceConfig = protection // {
      SupplementaryGroups = [ "uinput" ];
      ExecStart = "${python}/bin/python ${./apple_remote_mouse.py}";
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
      Environment = "PATH=/etc/profiles/per-user/kacper/bin:/run/current-system/sw/bin:${pkgs.bluez}/bin:${pkgs.pipewire}/bin:${pkgs.wireplumber}/bin";
      AmbientCapabilities = [ "CAP_NET_RAW" "CAP_NET_ADMIN" ];
      CapabilityBoundingSet = [ "CAP_NET_RAW" "CAP_NET_ADMIN" ];
      ExecStart = "${python}/bin/python ${./apple_remote_mic.py} --direct-monitor";
    };
  };
}
