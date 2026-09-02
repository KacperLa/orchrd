# Orchard

Orchard is a GNOME 50 voice control surface for persistent Codex sessions in
Herdr. Hold a global shortcut, speak, and Orchard asks the configured intent
model to carry on a bounded, tool-using conversation and operate on the right
Herdr project. When an agent finishes,
Orchard either speaks a short summary with Pocket TTS or queues it behind a
notification sound.

The implementation has three runtime pieces:

- `orchardd`, the Rust D-Bus service and orchestrator;
- `orchard-tts`, a persistent local Pocket TTS worker;
- `orchard@kacper.local`, the GNOME Shell extension.

Voxtype, Herdr, and Codex are external prerequisites. The Orchard flake never
installs or replaces them.

## Configuration

Copy `config.example.toml` to `~/.config/orchard/config.toml` and edit it. The
API key named by `intent.api_key_env` must be present in the systemd user
service environment. Project paths must already exist and are canonicalized at
startup.

Pocket TTS code is included in the Nix package, while model and voice assets
remain user data. Download the model assets once, copy the matching Pocket TTS
YAML locally, replace its `hf://` weight/tokenizer references with those local
files, and point `tts.model_config` at that YAML. Point `tts.voice` at a local
reference WAV or exported `.safetensors` voice state. Orchard performs no
runtime package installation; using an unchanged upstream YAML may let Pocket
TTS fetch its referenced model files from Hugging Face on first start.

Orchard stores user utterances, its spoken replies, agent questions, and task
completion summaries in SQLite. Each model turn receives the most recent
`conversation.max_messages` entries verbatim. Older entries remain stored but
are not compacted or sent to the model.

The GNOME shortcuts are GSettings keys:

```console
gsettings set org.gnome.shell.extensions.orchard push-to-talk "['Print']"
gsettings set org.gnome.shell.extensions.orchard speak-next "['<Ctrl><Alt><Shift>space']"
```

## NixOS/Home Manager

Add this repository as a flake input, expose its Home Manager module as a shared
module, and enable the service for the user:

```nix
inputs.orchard.url = "path:/home/kacper/Documents/Projects/orchard";

# Inside the NixOS module list:
{
  home-manager.sharedModules = [ orchard.homeManagerModules.default ];
  home-manager.users.kacper.services.orchard = {
    enable = true;
    configFile = "/home/kacper/.config/orchard/config.toml";
  };
}
```

Add `"orchard@kacper.local"` to the existing
`org/gnome/shell.enabled-extensions` list. The module deliberately does not
overwrite that list because this machine already manages it declaratively.

Build and validate with:

```console
nix flake check
nix build
```

## Diagnostics

```console
orchardctl status
orchardctl dispatch "what is Orchard working on"
orchardctl mode auto
orchardctl mode queued
orchardctl speak-next
```

The daemon stores operational state in
`$XDG_DATA_HOME/orchard/orchard.sqlite3` and temporary transcripts/sockets under
`$XDG_RUNTIME_DIR/orchard`.
