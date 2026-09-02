self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.orchrd;
  servicePath = "%h/.nix-profile/bin:/etc/profiles/per-user/%u/bin:/run/current-system/sw/bin";
in
{
  options.services.orchrd = {
    enable = lib.mkEnableOption "Orchrd voice control";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
    };
    configFile = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.configHome}/orchrd/config.toml";
    };
    herdrCommand = lib.mkOption {
      type = lib.types.str;
      default = "herdr";
    };
    herdrSession = lib.mkOption {
      type = lib.types.str;
      default = "orchrd";
    };
    enableHerdrServer = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.orchrd-tts = {
      Unit = {
        Description = "Orchrd Pocket TTS worker";
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
        ConditionPathExists = cfg.configFile;
      };
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/orchrd-tts --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    systemd.user.services.orchrd-herdr = lib.mkIf cfg.enableHerdrServer {
      Unit = {
        Description = "Headless Herdr session for Orchrd";
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

    systemd.user.services.orchrd = {
      Unit = {
        Description = "Orchrd voice-agent controller";
        PartOf = [ "graphical-session.target" ];
        ConditionPathExists = cfg.configFile;
        After = [
          "graphical-session.target"
          "voxtype.service"
          "orchrd-tts.service"
        ]
        ++ lib.optional cfg.enableHerdrServer "orchrd-herdr.service";
        Wants = [ "orchrd-tts.service" ] ++ lib.optional cfg.enableHerdrServer "orchrd-herdr.service";
      };
      Service = {
        Type = "dbus";
        BusName = "io.github.kacper.Orchrd1";
        ExecStart = "${cfg.package}/bin/orchrdd --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
