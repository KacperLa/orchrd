# Evaluation checks for the Home Manager and NixOS modules. The Apple TV
# remote is a standalone flake under ../apple_tv_remote and is checked there.
{
  self,
  home-manager,
  pkgs,
}:
let
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
assert services.orchrd.Unit.ConditionPathExists == "/home/alice/.config/orchrd/config.toml";
assert services.orchrd.Service.EnvironmentFile == [ "/run/secrets/orchrd" ];
assert
  builtins.attrNames services == [
    "orchrd"
    "orchrd-tts"
  ];
assert
  builtins.attrNames standalone.options.services.orchrd == [
    "configFile"
    "enable"
    "enablePocketTts"
    "environmentFile"
    "package"
  ];
pkgs.runCommand "orchrd-module-evaluation" { } "touch $out"
