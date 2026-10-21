<p align="center">
  <img src="extension/icons/orchrd-symbolic.svg" width="132" height="132" alt="Orchrd forest icon">
</p>

<h1 align="center">Orchrd</h1>

<p align="center">
  <strong>A voice-first control surface for persistent coding agents.</strong>
</p>

<p align="center">
  Connect Codex or NOBS sessions, hold a key, and move work forward by voice.
</p>

Orchrd turns GNOME into a lightweight voice interface for software work. It
uses a configured speech-to-text API and Pocket TTS by default,
classifies your request with a System One decision model, and
routes work across connected Codex and NOBS sessions. When a coding agent needs input, Orchrd asks
you. When the work finishes, it can speak a short summary or stay silent and
show a notification.

Orchrd is deliberately small. It is not another coding agent and it does not
replace your terminal workflow. It is the layer that keeps several projects
moving while your hands and attention are elsewhere. Codex and NOBS own sessions
and agents; Orchrd has no separate project registry.

## How it works

### Herdr backend (experimental)

Orchrd can now discover coding agents directly from Herdr, without a wrapper or
agent connector. Set the following in your runtime configuration and restart
Orchrd:

```toml
[agents]
backend = "herdr"
herdr_command = "herdr" # An absolute executable path also works.
herdr_session = "default"
screen_lines = 30
poll_seconds = 2
```

Run Herdr normally with its agents. Orchrd polls the roster and visible terminal
text. Two background workers process changed, settled screens: System One checks
for a current dialog or assistant question, then the `[intent]` model extracts
the question, choices, type, and permission scopes. Literal grounding,
completeness, and a fresh-screen check gate presentation. The prompts and schema
are shared across agents; there are no per-agent terminal parsers in Orchrd.

Open `orchrd-activity` to see detection/extraction progress and the current
questions in the **Agent questions** panel. `orchrdctl status` includes them
under `observed_questions`, with the full extraction and input capabilities.
`read_only: true` now identifies an individual question whose controls could not
be established; it does not disable the backend.
`orchrdctl mode auto` enables automatic speech; muted mode still displays the
questions. Asking Orchrd to read a named agent reads an available question with
all its choices. `orchrdctl rescan` retries unchanged screens that did not yield
a question; otherwise each unchanged screen is attempted once. Question
extraction can take roughly 20–60 seconds with the currently tested model.
Live testing has also returned a generic Enter/Escape confirmation instruction
as the question itself. Grounding and completeness checks do not prove semantic
correctness or that every permission scope was captured; compare the observed
question with the terminal while testing. The live trial needed about 65 seconds
for one extraction, so a longer `[intent].timeout_seconds` may be needed.

The backend supports sending work, replying to chat questions, choosing menu
options, approvals/denials, and cancellation. Prompts use `herdr agent prompt`;
menu and form input uses `agent send-keys`, never a shell command. The controller
binds questions to the capture present when speech starts, checks agent identity
and the screen again immediately before writing, consumes question IDs once,
and never automatically retries uncertain input.

Say **approve** to choose an unambiguous **allow once** option, **deny** to select
the denial option, or name a specific option/number. Persistent permission is
available only by explicitly selecting that option; generic approval never
promotes permission scope. Approval buttons are offered when both approval and
denial controls are supported. Cancellation sends `ctrl+c` once.

Menus require visible shortcuts or grounded focus with explicit Up/Down and
Enter instructions. Checkbox menus additionally require visible checked states
and Space/Enter controls; the controller toggles only states that differ from
the requested selection. Modal text fields require visible Enter submission;
chat questions use ordinary prompt submission. Unsupported controls stay in the
terminal fallback. Text replies never become key names or shell syntax.

Herdr's current input API has no atomic expected-screen condition. The final
read reduces the race window but cannot eliminate concurrent user input or UI
changes between that read and the write. Model grounding is not a semantic
proof. Input delivery receipts do not claim task completion; the controller
reports observed lifecycle transitions separately. Normal read requests can
summarize terminal output; automatic completion summaries are still separate
work for this backend.

Use either this backend or the structured bridge; they are not merged. For Home
Manager, keep `services.orchrd.enableCodex = false` while using Herdr, and ensure
`herdr_command` is available in the user service's PATH. Select
`backend = "bridge"` and restart to return to the existing connectors. Existing
configurations default to the bridge backend.

See the [pipeline evaluation](experiments/dialog_detector/PIPELINE.md) for the
measured results and limitations. The flow below describes the bridge backend.

```mermaid
flowchart LR
    Voice[You speak] --> Input[GNOME shortcut or Apple remote]
    Input --> Capture[Controller: PipeWire capture]
    Capture --> STT[Transcription API]
    STT --> Router[Orchrd controller]
    Router --> Decision[Kev: operation and target]
    Decision --> Draft[LLM: request text only]
    Draft --> Execute[Controller: execute once]
    Execute --> Bridge[Local agent bridge]
    Bridge <--> NOBS[Patched NOBS sessions]
    Bridge <--> Adapter[orchrd-codex adapter]
    Adapter <--> Server[Shared Codex app server]
    Terminal[orchrd codex terminal] <--> Server
    NOBS --> Summary[Response summary]
    Server --> Summary
    Summary --> TTS[TTS provider]
    TTS --> Voice
```

1. Hold <kbd>Print Screen</kbd> while speaking.
2. Orchrd captures the PipeWire microphone and sends the audio to the configured
   transcription API after key release.
3. Kev chooses the operation and named target. The controller resolves target
   fallbacks from the live roster and last-interacted agent.
4. Orchrd sends a cleaned, non-speculative request to the appropriate connected agent.
5. If an agent asks a question, Orchrd reads its numbered choices and sends your
   reply to that pending question. Permission requests use separate approve/deny
   controls. Delivery acknowledgements are based on the frontend's receipt and
   observed state changes.
6. When an agent responds, Orchrd summarizes the structured final message from
   the connected frontend, including turns started directly in its terminal.
7. Depending on the selected mode, the selected TTS provider speaks the result immediately or
   Orchrd stays muted and plays a notification sound.

After selecting an agent for a send operation, a separate LLM call writes only
its `request` text from the current utterance and selected recipient. It has no
conversation transcript or routing tools. The controller supplies the agent ID,
validates the draft, and sends it once; drafting does not confirm delivery.

Kev classifies the current utterance into an operation and a named target in
one System One request. It receives the utterance, connected agent names, and live pending questions,
with `unspecified` and `unknown` target choices. The controller resolves an
unspecified target to the sole connected agent or the last-interacted connected
agent. Missing or duplicate names require clarification. Confidence scores are
recorded for inspection; no unvalidated confidence threshold is applied.

The `[routing]` configuration selects the System One endpoint, model, optional
Bearer API key, and timeout. These settings apply to routing and question choices.
The `[intent]` LLM drafts requests and summarizes agent output; it no longer
selects tools or generates delivery acknowledgements. Routing failure stops the
turn, without falling back to an unconstrained LLM. Each utterance executes at
most one operation; multi-agent instructions should be issued separately.
Previous Orchard replies, summaries, and receipts are never replayed as routing
memory. History remains in SQLite for inspection. Legacy `[conversation]`
settings remain accepted for compatibility but no longer control a tool loop.

Sending work, reading agent output, approving or denying a prompt, and
cancelling work are separate tool operations. Codex and NOBS create and own their sessions;
Orchrd interacts only with agents already present. Tool results distinguish message
delivery from an observed agent response: delivery alone does not prove that work
started or an approval succeeded. Unconfirmed operations are never automatically
resent. Spoken output is also normalized
in code to plain text of at most two sentences, without Markdown or numbered
lists.

### Runtime architecture

One coordinator owns application state. Connected frontends are the source of
live agent status; Orchrd does not maintain a second set of coding-task states.

- Each interactive NOBS process connects through a private local socket,
  registers as one agent in the shared roster, and sends authoritative state,
  final responses, and structured permission requests. Orchrd sends prompts,
  cancellation, and exact permission selections back over that connection; it
  never scrapes terminal output.
- For Codex, the `orchrd codex` wrapper opens a terminal client on the shared
  app server. The separate `orchrd-codex` adapter connects to that same server
  over a Unix WebSocket and registers loaded top-level threads with the local
  agent bridge. Terminal and voice commands operate on the same sessions.
- A summary worker processes those responses, appends each summary to the
  shared conversation history once, and queues speech when auto-speak is on.
  One speech worker plays responses in order. Slow model calls or audio playback
  do not block status observation.
- User turns are processed serially. Connected agents may appear or disappear
  between turns; the model receives a refreshed roster before each call.
- Mutating tools register a pending operation before sending to an agent. Their
  promises resolve from delivery receipts and observed status changes, or expire
  with an explicit unconfirmed outcome. Sending to an already-working agent
  confirms delivery only, not that the new request has begun.
- Approval controls carry the exact pending question ID. A voice turn remembers
  which permissions were pending when it began, so a delayed response cannot
  approve a newer request. Bridge messages and work queues have explicit limits;
  a full speech queue leaves the response available in history.

Pending summaries survive service restarts, but old audio is not replayed.
Interrupted deliveries are recorded as unconfirmed and are not retried. Frontends
send final output and permission requests directly; Orchrd polls its local
bridge only for the latest roster and status snapshot.

## Components

| Component | Responsibility |
| --- | --- |
| `orchrdd` | Rust controller, deterministic dispatch, state reconciliation, approvals, and D-Bus API |
| `orchrd codex` | Terminal launcher connecting Codex to the shared app server |
| `orchrd-codex` | Adapter registering shared Codex threads with the local agent bridge |
| GNOME extension | Panel menu, push-to-talk shortcuts, status, and notifications |
| `orchrd-activity` | Native GTK4 activity window rendering coordinator snapshots over D-Bus |
| `orchrd-tts` | Persistent local Pocket TTS worker with ordered playback |
| STT API | Speech-to-text from PipeWire microphone capture |
| NOBS | Interactive coding-agent TUI; a patched local process registers itself with Orchrd |
| Apple remote services | Optional Bluetooth touchpad and microphone input; source and Nix module in `apple_tv_remote/` |
| Kev | System One classification of operation and named target |
| Request/summary LLM | Drafts request text and summarizes agent output |
| Coding models | Coding, repository inspection, and implementation inside Codex or NOBS |

Use a shared Codex app server, a patched NOBS build, or both. Orchrd does not
install or configure the coding models used by either frontend.

## Requirements

- Nix with flakes enabled
- Linux with Home Manager; NixOS is required for the optional Apple TV remote services
- GNOME Shell 50 or 51 (including 51 prereleases)
- An OpenAI-compatible audio transcription API
- A shared Codex app server with its adapter, or a patched NOBS build with native
  Orchrd bridge support, and at least one connected session
- A reachable System One endpoint configured in `[routing]`
- An OpenAI-compatible chat-completions endpoint for request drafting and summaries,
  with credentials if required by the provider
- Pocket TTS assets stored locally, or a compatible TTS API

## Setup

### 1. Add Orchrd to Home Manager

The root flake configures Orchrd through Home Manager. It does not import Home
Manager into your host or create system services. Add it to your existing flake:

```nix
inputs.orchrd = {
  url = "github:KacperLa/orchrd";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Import its module into your home configuration, passing the `orchrd` input into
scope through your flake's outputs or Home Manager's `extraSpecialArgs`:

```nix
{
  imports = [ orchrd.homeManagerModules.default ];
  services.orchrd = {
    enable = true;
    enableCodex = true;
    startCodexServer = true;
    # codexCommand = "/absolute/path/to/codex";
    # environmentFile = "/run/secrets/orchrd";
  };
}
```

With Home Manager integrated into NixOS, the same import can be written as:

```nix
{
  home-manager.users.you = {
    imports = [ orchrd.homeManagerModules.default ];
    services.orchrd.enable = true;
  };
}
```

Keep your existing Home Manager installation and `home.stateVersion`. Apply your
configuration with `home-manager switch --flake .#you`, or your normal
`nixos-rebuild switch --flake .#my-host` when using Home Manager through NixOS.
After rebuilding, enable Orchrd using the GNOME Extensions application.

The module defaults to `~/.config/orchrd/config.toml`. Install Codex separately
so `codex` is on the service PATH, or set `codexCommand` to its absolute path.
The configuration file, provider credentials, and local TTS assets are supplied
in the following steps. Normal microphone capture uses your desktop PipeWire
session and needs no Orchrd NixOS module.

The root flake exports `homeManagerModules.default` and
`packages.<system>.default`. To install only the tools, add
`orchrd.packages.${pkgs.system}.default` to `home.packages`.
The optional Apple TV remote has its own [NixOS flake](apple_tv_remote/flake.nix),
configured separately below.

The module installs Orchrd into the user profile and creates two user services:

- `orchrd.service` owns the conversation loop, agent bridge, speech integration,
  persistent state, and D-Bus API used by the extension.
- `orchrd-tts.service` keeps Pocket TTS loaded and serializes audio playback.

NOBS sessions register with Orchrd while both programs run. Orchrd does not
start or stop NOBS, and no session selection is needed.
For Codex, enable the adapter and optionally the shared app-server service as
described in [Codex sessions](#codex-sessions).

### 2. Create the configuration file

Copy [config.example.toml](config.example.toml) to
`~/.config/orchrd/config.toml` and edit it manually:

```bash
mkdir -p ~/.config/orchrd
cp /path/to/orchrd/config.example.toml ~/.config/orchrd/config.toml
```

At minimum, configure the conversational endpoint, model, and local TTS assets:

```toml
[intent]
base_url = "https://api.openai.com/v1"
model = "gpt-5-mini"
api_key_env = "ORCHRD_API_KEY"

[routing]
endpoint = "http://127.0.0.1:8009/v1/systemone"
model = "kev-latest"
api_key = "" # Set the decision API's Bearer token here if required.
timeout_seconds = 30

[voice]
announcement_mode = "muted"

[stt.api]
base_url = "http://127.0.0.1:8000/v1" # Replace with your STT API root.
model = "your-stt-model"
timeout_seconds = 60

[tts]
language = "english_2026-04"
model_config = "/home/you/.local/share/orchrd/pocket-tts/english_2026-04.yaml"
voice = "/home/you/.local/share/orchrd/pocket-tts/voice.safetensors"
quantize = true
```

No project paths or discovery roots are required. Launch `orchrd codex` against
the shared app server, or start the patched interactive NOBS binary normally.
The Codex adapter discovers loaded threads; NOBS instances register themselves.

`routing.endpoint` is the complete decision API URL, including `/v1/systemone`;
`routing.url` is an alias. An omitted or empty `routing.api_key` sends no
Authorization header. Restart `orchrd.service` after changing these settings.

When updating an older configuration, remove `[commands].voxtype`,
`[stt].provider`, `[voice].voxtype_state_file`, and
`[voice].transcription_timeout_seconds`, then configure `[stt.api]`.
`stt.api.timeout_seconds` controls the transcription request; microphone capture
is limited to two minutes. Remove `[summary].cloud_context` and `include_diff`.
Existing SQLite conversation history is preserved. Former run and agent-binding
tables are no longer used; live agents are discovered directly from their bridge
connections.

### 3. Provide the API key

The variable named by `intent.api_key_env` must be available to the systemd
user manager, not only to an interactive shell. A convenient local option is
an environment file such as `~/.config/environment.d/90-orchrd.conf`:

```text
ORCHRD_API_KEY=your-key-here
```

Log out and back in after creating it, or import the variable into the current
user manager before restarting Orchrd. Do not place secrets directly in a Nix
module because Nix store contents are world-readable.

### Optional: use hosted speech APIs

STT always uses the configured audio transcription API, which may be local or
hosted. TTS uses Pocket TTS by default or an OpenAI-compatible speech API.
For example, configure hosted STT and TTS with:

```toml
[stt.api]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini-transcribe"
api_key_env = "ORCHRD_STT_API_KEY"
timeout_seconds = 60

[tts]
provider = "openai_compatible"

[tts.api]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini-tts"
voice = "coral"
api_key_env = "ORCHRD_TTS_API_KEY"
timeout_seconds = 120
```

These adapters use the [audio transcription API](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create)
and [speech API](https://developers.openai.com/api/docs/guides/text-to-speech).
Other providers must implement those contracts; a compatible chat endpoint alone
is not enough. STT requires multipart WAV upload and a JSON `text` response. TTS
requires streaming raw **24 kHz, mono, signed 16-bit little-endian PCM**. Set each
`base_url` to the API root (including `/v1` if required), not the audio endpoint.
For an unauthenticated local server, omit `api_key_env`. Use HTTPS for remote
servers. Models and voice names depend on the provider.

Push-to-talk and its shortcut are unchanged. Orchrd captures the default
PipeWire microphone into memory, stops on key release, then uploads a WAV.
Recordings are limited to two minutes and discarded after transcription or
cancellation; no recording file is stored. STT is **not live-streaming**. Remote
TTS plays incoming audio chunks without buffering the whole utterance, and PTT
interrupts playback. Errors surface normally; requests are not silently retried
against another provider.

Remote STT sends your microphone audio to that provider; remote TTS sends the
spoken response text. These services may incur charges and have their own data
retention policies. Generated speech is synthetic, not a human recording.

For provider TTS, disable the unused local worker in Home Manager:

```nix
services.orchrd = {
  enable = true;
  enablePocketTts = false; # Keep true when tts.provider = "pocket".
  environmentFile = "${config.home.homeDirectory}/.config/orchrd/secrets.env";
};
```

Create that file outside the Nix store, restrict it to your user (`chmod 600`),
and set the variables named in your config, e.g. `ORCHRD_STT_API_KEY=...` and
`ORCHRD_TTS_API_KEY=...` (plus your intent API key if needed). Rebuild your Home
Manager configuration and restart `orchrd.service`. PipeWire tools are provided
by the flake; no imperative installation is needed. Skip the next asset step
when not using Pocket TTS. The package still includes Pocket TTS, even when its
worker is disabled.

### 4. Prepare Pocket TTS assets (local TTS only)

Create a user-data directory and place the Pocket TTS assets there:

```text
~/.local/share/orchrd/pocket-tts/
├── english_2026-04.yaml
├── model weights referenced by the YAML
├── tokenizer assets referenced by the YAML
└── voice.safetensors
```

The model YAML must reference the corresponding local files. The `tts.voice`
setting accepts a compatible reference WAV or exported `.safetensors` voice
state. Orchrd packages the Pocket TTS program, but keeps large model and voice
assets outside the Nix store as user data.

### 5. Build and activate

Update Orchrd's lock entry to fetch the latest revision, then rebuild:

```bash
cd /path/to/your/nixos-config
nix flake update orchrd
sudo nixos-rebuild switch --flake .#your-host
```

Reload the extension after an extension-only change:

```bash
orchrd_uuid="$(gnome-extensions list | grep '^orchrd@' | head -n1)"
gnome-extensions disable "$orchrd_uuid"
gnome-extensions enable "$orchrd_uuid"
```

### 6. Verify the services

```bash
systemctl --user status orchrd.service
systemctl --user status orchrd-tts.service
orchrdctl doctor
orchrdctl status
```

## Using Orchrd

The default push-to-talk key is <kbd>Print Screen</kbd>. Hold it while speaking
and release it to transcribe and route the request.

The Codex adapter and patched NOBS instances register their live sessions. Orchrd monitors all connected
sessions and shows them in the panel without a picker.

Orchrd also watches responses from turns you start directly in Codex or NOBS,
not just requests sent by voice. The frontends send final responses and
permission requests as structured events. Orchrd records a short summary
and speaks it if **Auto speak results** is enabled. It does not read terminal
scrollback.

The panel shows the controller state and each connected agent on its
own row. It also provides:

- **Open activity** to inspect live agent lanes and the voice/routing pipeline
- **Auto speak results** to switch between immediate speech and muted notifications
- **Interaction sounds** to enable or disable push-to-talk progress cues
- **Repeat last response** to hear the last thing Orchrd spoke again

With interaction sounds enabled, stopping recording plays a short confirmation
tone. Finishing transcription plays a brighter ping followed by a two-pulse
routing cue. These are one-shot sounds rather than loops, so a slow model call
does not produce continuous audio.

Muted completions are recorded normally but are not placed in a pending-results
queue. Ask Orchrd for an agent status when you want to catch up.
**Repeat last response** replays the most recent utterance that finished speaking,
and remains available after a service restart. If multiple spoken responses
arrive together, Orchrd plays them in order without interrupting either one.
Starting push-to-talk is the explicit interruption: it stops current speech and
clears any audio still waiting to play.

### Codex sessions

`orchrd-codex` connects to a shared local Codex app server over a Unix WebSocket
and registers its loaded top-level threads with Orchard. It supports prompts,
steering an observed active turn, cancellation, completion announcements,
command/file approvals, and ordinary user questions. Multiple questions are
answered one at a time; the combined answer goes back to the original Codex
request. An answer made in the Codex terminal clears the pending voice question.

Sessions appear under their Codex titles, and renaming a session updates its
display name in Orchard. Untitled sessions use their working directory name,
or `Codex session` when no directory is available. Internal IDs keep routing
distinct sessions with the same title.

The terminal and adapter must use **the same app server**. This does not attach
to an arbitrary independent Codex process. Existing history is not read aloud
when attaching. Loaded idle sessions remain available; archived/closed sessions
are removed when Codex unloads them. Spawned subagents are excluded.
New empty sessions become available after Codex has saved their first history.

For Home Manager:

```nix
services.orchrd = {
  enableCodex = true;
  startCodexServer = true;
  # codexCommand = "/absolute/path/to/codex"; # defaults to codex on PATH
};
```

After rebuilding, start the services and open a terminal client on their socket:

```bash
systemctl --user start orchrd-codex-server.service orchrd-codex.service
orchrd codex
```

`orchrd codex` supplies the shared-server address and forwards additional Codex
arguments unchanged, for example `orchrd codex resume`. It keeps the current
directory and terminal. The server must already be running. Home Manager sets
the launcher socket and executable to match the bridge configuration; load the
updated session environment after rebuilding. Without Home Manager, the default
is `$XDG_RUNTIME_DIR/orchrd-codex/server.sock`; override it with
`ORCHRD_CODEX_SOCKET=/path/to/socket` and optionally `ORCHRD_CODEX_COMMAND`.

If you already run a shared Codex daemon, leave `startCodexServer = false` and
set `codexSocket` to its control socket if it differs from
`~/.codex/app-server-control/app-server-control.sock`.

Without Home Manager, run each long-lived process in a separate terminal:

```bash
# Server (skip if your shared Codex daemon is already running):
codex app-server --listen "unix://$XDG_RUNTIME_DIR/orchrd-codex.sock"
# Orchard adapter:
orchrd-codex --codex-socket "$XDG_RUNTIME_DIR/orchrd-codex.sock"
# Interactive client:
ORCHRD_CODEX_SOCKET="$XDG_RUNTIME_DIR/orchrd-codex.sock" orchrd codex
```

Use `orchrd-codex --codex-socket /path/to/socket` for a custom server socket.
The adapter reconnects after either service restarts and never retries a prompt
whose delivery is uncertain. It preserves Codex's model, credentials, and
permission configuration. Secret questions and unsupported permission types
(including scoped filesystem/network grants and MCP forms) stay in the Codex
terminal. Voice approval grants only the current command or file-change request.
If the adapter joins mid-turn without its turn ID, it waits for the next observed
turn before offering steering/cancellation.

Protocol reference: [Codex app server](https://learn.chatgpt.com/docs/app-server).
The installed Codex must support Unix WebSockets and the shared-thread APIs;
these interfaces are experimental. The adapter uses Python with `websockets`,
included in the Nix package.

Test the adapter with `python3 -m unittest discover -s adapters`. An optional
real-server smoke test uses a temporary Codex home and local fixture history,
and sends no inference:

```bash
nix develop --command bash -c 'ORCHRD_CODEX_SMOKE=1 "$ORCHRD_BRIDGE_PYTHON" -m unittest discover -s adapters -v'
```

### NOBS sessions

The patched local NOBS checkout connects to Orchrd automatically from ordinary
interactive mode. Each process receives a unique `nobs:<pid>:<random>` identifier
in Orchrd's roster. Text entered in NOBS and requests sent by voice reach the
**same session and transcript**. NOBS reports exact lifecycle state, final assistant
text, and structured permission requests. Approval commands carry the exact
pending question ID; stale controls are rejected. Voice approvals select its real
`allow_once` or `reject_once` option. For multiple-choice questions, Orchrd reads
all numbered options without summary truncation. Reply with an option number,
label, multiple selections when offered, or your own answer when allowed.
Kev first classifies your reply as an option selection, custom answer, or
clarification. For selections, a second Kev call decides which offered options
your utterance includes. These calls receive only the current reply and question
data; custom answers preserve your utterance verbatim. Both bridges validate
the option IDs and question ID before accepting an answer; a stale answer cannot resolve a newer question. Ordinary questions never use
the yes/no permission controls. The normal NOBS TUI remains fully usable.

After rebuilding NOBS and Orchrd, restart `orchrd.service` and start NOBS normally.
Do not launch `nobs --acp`; ACP mode is a separate frontend and does not run the TUI.

Example requests:

```text
Ask the reviewer to inspect the failing tests.
What did the agent just finish?
Approve the request.
Ask it why that test is failing.
```

To change the shortcuts:

```bash
gsettings set org.gnome.shell.extensions.orchrd push-to-talk "['Print']"
```

### Apple TV remote input

[`apple_tv_remote/`](apple_tv_remote/) contains copies of the deployed Apple TV
remote mouse and microphone scripts, plus a configurable NixOS service module. The scripts
match the Nix store versions used by the running services when copied into this
repository. They are separate system services from the Home Manager user services.

- `apple_remote_mouse.py` connects the Bluetooth remote and translates touchpad
  reports into mouse input through uinput.
- `apple_remote_mic.py` decodes the remote's Opus audio into a virtual PipeWire
  microphone. Holding Siri selects that microphone and calls `orchrdctl start`;
  releasing Siri calls `orchrdctl stop` after draining the audio, then restores
  the previous input unless the user selected another source during dictation.
- `apple-remote.nix` configures both services, device access, dependencies, and
  Bluetooth monitoring capabilities.

The remote has a separate flake under `apple_tv_remote/`. Add this input to your
NixOS host flake (the `dir` selects the remote flake, rather than the Orchrd flake):

```nix
inputs.apple-tv-remote = {
  url = "github:KacperLa/orchrd?dir=apple_tv_remote";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Import its module in your NixOS configuration, with `apple-tv-remote` in scope
from your flake's outputs:

```nix
{
  imports = [ apple-tv-remote.nixosModules.default ];
  users.users.you.uid = 1000; # Use your user's existing numeric UID.
  services.apple-tv-remote = {
    enable = true;
    user = "you";
    macAddress = "AA:BB:CC:DD:EE:FF"; # Your paired remote's Bluetooth address.
  };
}
```

The module derives the runtime directory and profile path from that user and
passes the configured MAC to both scripts. Pair the remote first. The microphone
service needs the user's PipeWire session and an installed `orchrdctl`, provided
by the Orchrd Home Manager setup above. Rebuild NixOS after configuring the remote.
The remote flake does not import Home Manager or configure Orchrd.

## Native activity window

Launch **Orchrd Activity** from GNOME, select **Open activity** in the panel,
or run `orchrd-activity`. It uses GTK4 and Cairo through PyGObject—no browser,
web server, or separate model interpreting logs.

The graph has an Orchrd lane and a lane per agent. Endpoints render the actual
frontend status: blue/spinning for working, gray for idle, green/pulsing for done,
and amber for blocked. Status-transition nodes retain what the frontend reported;
delivery acceptance never changes an agent's endpoint.

Coordinator-owned nodes expose utterance/transcription, model waiting, exact tool
arguments, delivery and confirmation, summary persistence, and
queued/playing/skipped speech. Curves connect explicit parent operation IDs;
they do not guess which user request caused independently observed agent output.
Action-specific vector glyphs sit inside the nodes; status and elapsed time are
centered underneath. Hover for the full action name. Stream names stay in a sticky footer.
Click a node (or use the inspector dropdown and arrow keys) to see its data,
timestamps, and transitions. **Follow** keeps the latest activity visible.

The window consumes the same versioned snapshot as the panel and resynchronizes
when the daemon reconnects. Activity is recent, in-memory diagnostic state for
the live agent roster, not a new persistent history store: daemon restarts clear
it. At most 200 records are retained unless more operations
are still pending. Large detail payloads are explicitly marked as truncated.
The underlying conversation history and captured-response queue remain in SQLite.

This displays tool decisions and their observable results, not hidden model
reasoning. Frontends send responses directly; the current agent roster and status
are sampled from the local bridge.

## Command-line controls

`orchrdctl` exposes the same D-Bus controller used by the extension:

```bash
orchrdctl status
orchrdctl dispatch "what is Orchrd working on?"
orchrdctl mode auto
orchrdctl mode muted
orchrdctl repeat-last
orchrdctl start
orchrdctl stop
orchrdctl cancel
```

## Diagnostics

Run `orchrdctl doctor` (or `orchrdctl doctor /path/to/config.toml`) before starting
services to check endpoint URLs, positive timeouts, API credentials, and readable,
nonempty local TTS configuration and voice files. It runs without D-Bus or a
running daemon; it does not contact providers or load model weights. The daemon
performs these checks at startup too. Remote TTS skips local asset checks. Set
`intent.api_key_env = ""` for an unauthenticated local intent service.

Follow controller logs, including failed conversational tool calls:

```bash
journalctl --user -u orchrd.service -f
```

Every conversational tool call is logged as selected and then as succeeded or
failed. Completion summarization is a separate model call. If that call returns
invalid output, the coding task remains complete and Orchrd records a short
fallback message rather than treating the task itself as failed.

Inspect the other runtime services:

```bash
journalctl --user -u orchrd-tts.service -f
```

Runtime data lives in standard XDG locations:

- Conversation history, captured responses, delivery receipts, and settings:
  `$XDG_DATA_HOME/orchrd/orchrd.sqlite3`
- Temporary transcripts and the TTS socket: `$XDG_RUNTIME_DIR/orchrd/`
- User configuration: `$XDG_CONFIG_HOME/orchrd/config.toml`

## Development

From a fresh terminal, run all tests and code checks with:

```bash
./scripts/test.sh
```

The script enters the pinned Nix development shell automatically and runs Rust
formatting checks, tests, Clippy, GNOME extension tests, coding-agent adapter tests, TTS worker tests, and
headless GTK viewer tests. It stops at the first failure without reformatting
files or restarting services. No API keys or running Orchrd instance are needed. Tests include untracked working-tree files and skip call-graph regeneration.
You can also invoke it by its absolute path from another directory. The first
run may download Nix dependencies; it does not install them into your profile.

For an interactive development shell, use `nix develop`.
In that shell, successful `cargo build`, `cargo check`, `cargo test`, and
`cargo clippy` commands regenerate the [static call graph](docs/call-graph/README.md).
`cargo run` and `cargo bench` regenerate it before starting the program.
You can also run `python3 scripts/regenerate_call_graph.py` in the shell.

Check the NixOS and Home Manager module integration without building the full
application with `nix build .#checks.x86_64-linux.module-evaluation --no-link`
(substitute your platform if needed). `nix flake check` includes this check and
the application package checks.

Build the complete package, including the extension and TTS worker:

```bash
nix build
nix flake check
```

Orchrd is currently an early, personal project. Its configuration and D-Bus
surface may change while the interaction model settles.

The application icon is adapted from Lucide's
[trees-forest](https://github.com/lucide-icons/lucide/blob/main/lab/trees-forest.svg)
icon, copyright Lucide Icons and contributors, and used under the ISC License.
