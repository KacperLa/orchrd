{
  lib,
  package,
  configFile,
}:
{
  enable = lib.mkEnableOption "Orchrd voice control";
  package = lib.mkOption {
    type = lib.types.package;
    default = package;
    description = "Orchrd package to install and run in the user's session";
  };
  configFile = lib.mkOption {
    type = lib.types.str;
    default = configFile;
    description = "Runtime TOML configuration file; keep credentials outside the Nix store";
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
}
