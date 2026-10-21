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
  options.services.orchrd = import ./service-options.nix {
    inherit lib;
    package = self.packages.${pkgs.system}.default;
    configFile = "${config.xdg.configHome}/orchrd/config.toml";
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    systemd.user.services.orchrd-tts = lib.mkIf cfg.enablePocketTts {
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

    systemd.user.services.orchrd = {
      Unit = {
        Description = "Orchrd voice-agent controller";
        PartOf = [ "graphical-session.target" ];
        ConditionPathExists = cfg.configFile;
        After = [
          "graphical-session.target"
        ]
        ++ lib.optional cfg.enablePocketTts "orchrd-tts.service";
        Wants = lib.optional cfg.enablePocketTts "orchrd-tts.service";
      };
      Service = {
        Type = "dbus";
        BusName = "io.github.kacper.Orchrd1";
        EnvironmentFile = lib.optional (cfg.environmentFile != null) cfg.environmentFile;
        ExecStart = "${cfg.package}/bin/orchrdd --config ${cfg.configFile}";
        Restart = "on-failure";
        RestartSec = 3;
        Environment = [ "PATH=${servicePath}" ];
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
