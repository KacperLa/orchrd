self: home-manager:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.orchrd;
  enabled = cfg.enable && cfg.user != null;
  account = config.users.users.${cfg.user};
  # Reuse the same service options for NixOS and standalone Home Manager.
  serviceOptions = import ./service-options.nix {
    inherit lib;
    package = self.packages.${pkgs.system}.default;
    configFile = if cfg.user == null then "" else "${account.home}/.config/orchrd/config.toml";
  };
in
{
  imports = [ home-manager.nixosModules.home-manager ];

  options.services.orchrd = serviceOptions // {
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "alice";
      description = "Existing NixOS user whose graphical session runs Orchrd";
    };
  };

  config = lib.mkMerge [
    { home-manager.sharedModules = [ self.homeManagerModules.default ]; }
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.user != null && builtins.hasAttr cfg.user config.users.users;
          message = "services.orchrd.user must name an existing NixOS user.";
        }
      ];
    })
    (lib.mkIf enabled {
      home-manager.users.${cfg.user} = {
        home.stateVersion = lib.mkDefault config.system.stateVersion;
        services.orchrd = lib.genAttrs (builtins.attrNames serviceOptions) (name: cfg.${name});
      };
    })
  ];
}
