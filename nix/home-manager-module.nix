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
  codexSocket = if cfg.codexSocket != null then cfg.codexSocket
    else if cfg.startCodexServer then "%t/orchrd-codex/server.sock"
    else "${config.home.homeDirectory}/.codex/app-server-control/app-server-control.sock";
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
    enablePocketTts = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run the local Pocket TTS worker; disable when tts.provider is openai_compatible";
    };
    enableCodex = lib.mkEnableOption "the bridge to existing shared Codex daemon sessions";
    startCodexServer = lib.mkEnableOption "a shared local Codex app server managed alongside the bridge";
    codexCommand = lib.mkOption {
      type = lib.types.str;
      default = "codex";
      description = "Codex executable used when startCodexServer is enabled";
    };
    codexSocket = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Shared Codex control socket; null uses the managed server socket when startCodexServer is enabled, otherwise Codex's default";
    };
    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Runtime environment file containing provider API keys (keep outside the Nix store)";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    home.sessionVariables = lib.mkIf cfg.enableCodex {
      ORCHRD_CODEX_SOCKET = codexSocket;
      ORCHRD_CODEX_COMMAND = cfg.codexCommand;
    };

    systemd.user.services.orchrd-codex-server = lib.mkIf (cfg.enableCodex && cfg.startCodexServer) {
      Unit = {
        Description = "Shared local Codex app server for Orchard and the terminal";
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = "${pkgs.writeShellScript "orchrd-codex-server" ''
          exec ${lib.escapeShellArgs [ cfg.codexCommand "app-server" "--listen" ]} "unix://$ORCHRD_CODEX_SOCKET"
        ''}";
        RuntimeDirectory = "orchrd-codex";
        RuntimeDirectoryMode = "0700";
        UMask = "0077";
        Environment = [ "PATH=${servicePath}" "ORCHRD_CODEX_SOCKET=${codexSocket}" ];
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

    systemd.user.services.orchrd-codex = lib.mkIf cfg.enableCodex {
      Unit = {
        Description = "Orchrd bridge to shared Codex sessions";
        After = [ "orchrd.service" ] ++ lib.optional cfg.startCodexServer "orchrd-codex-server.service";
        Wants = lib.optional cfg.startCodexServer "orchrd-codex-server.service";
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = lib.escapeShellArgs [ "${cfg.package}/bin/orchrd-codex" "--codex-socket" codexSocket ];
        Environment = [ "PATH=${servicePath}" ];
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };

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
          "voxtype.service"
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
