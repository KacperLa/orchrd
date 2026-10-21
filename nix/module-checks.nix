{
  self,
  nixpkgs,
  home-manager,
  pkgs,
  system,
}:
let
  remote = (import ../apple_tv_remote/flake.nix).outputs { self = remote; };
  host =
    extra:
    (nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        remote.nixosModules.default
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
  standalone = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
      {
        home = {
          username = "alice";
          homeDirectory = "/home/alice";
          stateVersion = "26.05";
        };
        services.orchrd = {
          enable = true;
          package = pkgs.hello;
          environmentFile = "/run/secrets/orchrd";
        };
      }
    ];
  };
  services = standalone.config.systemd.user.services;
in
assert !(disabled ? home-manager);
assert !(disabled.systemd.services ? apple-remote-mouse);
assert pkgs.lib.asserts.assertMsg (builtins.all (a: a.assertion) enabled.assertions) (
  builtins.toJSON (map (a: a.message) (builtins.filter (a: !a.assertion) enabled.assertions))
);
assert services.orchrd.Unit.ConditionPathExists == "/home/alice/.config/orchrd/config.toml";
assert services.orchrd.Service.EnvironmentFile == [ "/run/secrets/orchrd" ];
assert builtins.attrNames services == [ "orchrd" "orchrd-tts" ];
assert builtins.attrNames standalone.options.services.orchrd == [
  "configFile" "enable" "enablePocketTts" "environmentFile" "package"
];
assert enabled.systemd.services.apple-remote-mouse.serviceConfig.User == "alice";
assert pkgs.lib.hasInfix "--edge-mm 2 --corner-mm 4"
  enabled.systemd.services.apple-remote-mouse.serviceConfig.ExecStart;
assert enabled.systemd.services.apple-remote-mic.environment.XDG_RUNTIME_DIR == "/run/user/1234";
assert pkgs.lib.hasInfix "--mac AA:BB:CC:DD:EE:FF"
  enabled.systemd.services.apple-remote-mic.serviceConfig.ExecStart;
assert !(enabled.systemd.services ? orchrd);
pkgs.runCommand "orchrd-module-evaluation" { } "touch $out"
