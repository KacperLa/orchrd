self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.orchard;
  servicePath = "%h/.nix-profile/bin:/etc/profiles/per-user/%u/bin:/run/current-system/sw/bin";
in
{
  options.services.orchard = {
    enable = lib.mkEnableOption "Orchard voice control";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
    };
    configFile = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.configHome}/orchard/config.toml";
    };
    herdrCommand = lib.mkOption {
      type = lib.types.str;
      default = "herdr";
    };
    herdrSession = lib.mkOption {
      type = lib.types.str;
      default = "orchard";
    };
    enableHerdrServer = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.orchard-tts = {
      Unit = {
        Description = "Orchard Pocket TTS worker";
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
        ConditionPathExists = cfg.configFile;
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/orchard-tts --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    systemd.user.services.orchard-herdr = lib.mkIf cfg.enableHerdrServer {
      Unit = {
        Description = "Headless Herdr session for Orchard";
        PartOf = [ "default.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = "${pkgs.coreutils}/bin/env ${cfg.herdrCommand} --session ${cfg.herdrSession} server";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "default.target" ];
    };

    systemd.user.services.orchard = {
      Unit = {
        Description = "Orchard voice-agent controller";
        PartOf = [ "graphical-session.target" ];
        ConditionPathExists = cfg.configFile;
        After = [
          "graphical-session.target"
          "voxtype.service"
          "orchard-tts.service"
        ]
        ++ lib.optional cfg.enableHerdrServer "orchard-herdr.service";
        Wants = [ "orchard-tts.service" ] ++ lib.optional cfg.enableHerdrServer "orchard-herdr.service";
      };
      Service = {
        Type = "dbus";
        BusName = "io.github.kacper.Orchard1";
        ExecStart = "${cfg.package}/bin/orchardd --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
