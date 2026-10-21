# Evaluation checks for the NixOS module: verifies the option plumbing and the
# generated unit without building anything.
{
  pkgs,
  remote,
}:
let
  fakeFlake = {
    packages.${pkgs.stdenv.hostPlatform.system}.default = remote;
  };
  host =
    extra:
    (import (pkgs.path + "/nixos/lib/eval-config.nix") {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        (import ./module.nix { self = fakeFlake; })
        {
          system.stateVersion = "26.05";
          fileSystems."/" = {
            device = "/dev/root";
            fsType = "ext4";
          };
          boot.loader.grub.devices = [ "/dev/vda" ];
          users.users.alice = {
            isNormalUser = true;
            uid = 1234;
          };
        }
        extra
      ];
    }).config;
  disabled = host { };
  enabled = host {
    services.apple-tv-remote = {
      enable = true;
      user = "alice";
      macAddress = "AA:BB:CC:DD:EE:FF";
    };
  };
  service = enabled.systemd.services.apple-tv-remote;
in
assert !(disabled.systemd.services ? apple-tv-remote);
assert service.serviceConfig.User == "alice";
assert service.serviceConfig.AmbientCapabilities == [ "CAP_NET_RAW" ];
assert pkgs.lib.hasInfix "--mac AA:BB:CC:DD:EE:FF" service.serviceConfig.ExecStart;
assert pkgs.lib.hasInfix "OWNER=\"alice\"" enabled.services.udev.extraRules;
assert enabled.hardware.bluetooth.enable;
assert (builtins.head enabled.assertions).assertion;
pkgs.runCommand "apple-tv-remote-module-evaluation" { } "touch $out"
