{
  description = "Orchrd voice control for interactive coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    home-manager = {
      url = "github:nix-community/home-manager/release-26.05";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pocket-tts-src = {
      url = "github:kyutai-labs/pocket-tts/v2.1.0";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      home-manager,
      pocket-tts-src,
    }:
    let
      module = import ./nix/home-manager-module.nix self;
    in
    {
      homeManagerModules.default = module;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        py = pkgs.python3Packages;
        pocket-tts = py.buildPythonPackage {
          pname = "pocket-tts";
          version = "2.1.0";
          src = pocket-tts-src;
          pyproject = true;
          build-system = [ py.hatchling ];
          dependencies = with py; [
            numpy
            torch
            pydantic
            sentencepiece
            beartype
            safetensors
            typer
            fastapi
            uvicorn
            python-multipart
            scipy
            einops
            huggingface-hub
            requests
          ];
          doCheck = false;
        };
        pythonEnv = pkgs.python3.withPackages (_: [ pocket-tts ]);
        activityPython = pkgs.python3.withPackages (p: [ p.pygobject3 p.pycairo ]);
        activityTypelibs = pkgs.lib.makeSearchPath "lib/girepository-1.0" (map pkgs.lib.getLib [ pkgs.gtk4 pkgs.pango pkgs.harfbuzz pkgs.gdk-pixbuf pkgs.graphene pkgs.glib pkgs.gobject-introspection ]);
        callgraphTool = pkgs.rustPlatform.buildRustPackage {
          pname = "cargo-callgraph";
          version = "0.1.0";
          src = pkgs.fetchCrate {
            pname = "cargo-callgraph";
            version = "0.1.0";
            hash = "sha256-peziMUzp6oZdQsSxan4OwsiOH3OznOrTAEfnyd9e/+c=";
          };
          cargoLock.lockFile = ./nix/cargo-callgraph.lock;
          patches = [ ./nix/cargo-callgraph-impls.patch ];
          doCheck = false;
        };
        orchrd = pkgs.rustPlatform.buildRustPackage {
          pname = "orchrd";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                name = baseNameOf (toString path);
              in
              pkgs.lib.cleanSourceFilter path type
              && !(builtins.elem name [
                "target"
                "docs"
                "__pycache__"
                ".direnv"
                "result"
              ])
              && !(pkgs.lib.hasSuffix ".pyc" name);
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [
            pkgs.makeWrapper
            pkgs.glib
          ];
          nativeCheckInputs = [
            pkgs.gjs
            pkgs.python3
            pkgs.xvfb-run
            pkgs.dbus
          ];
          postCheck = ''
            gjs extension/test-lifecycle.js
            python3 -m unittest discover -s apple_tv_remote
            python3 -m unittest discover -s tts
            env -u GIO_EXTRA_MODULES -u GIO_MODULE_DIR GI_TYPELIB_PATH="${activityTypelibs}" GDK_BACKEND=x11 \
              XDG_DATA_DIRS="${pkgs.gtk4}/share" GTK_A11Y=none GIO_USE_VFS=local GTK_USE_PORTAL=0 \
              xvfb-run -a dbus-run-session --config-file=ui/test-bus.conf -- ${activityPython}/bin/python -m unittest discover -s ui
          '';
          postInstall = ''
            wrapProgram $out/bin/orchrdd \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.pipewire ]}
            install -d $out/libexec/orchrd
            install -m755 ui/orchrd_activity.py ui/orchrd_activity_render.py $out/libexec/orchrd/
            makeWrapper ${activityPython}/bin/python $out/bin/orchrd-activity \
              --add-flags "$out/libexec/orchrd/orchrd_activity.py" \
              --set GI_TYPELIB_PATH "${activityTypelibs}" \
              --prefix XDG_DATA_DIRS : "${pkgs.gtk4}/share:${pkgs.adwaita-icon-theme}/share"
            install -Dm644 ui/io.github.kacper.Orchrd.Activity.desktop $out/share/applications/io.github.kacper.Orchrd.Activity.desktop
            install -Dm644 extension/icons/orchrd-symbolic.svg $out/share/icons/hicolor/scalable/apps/orchrd.svg
            install -Dm755 tts/orchrd_tts.py $out/libexec/orchrd/orchrd_tts.py
            makeWrapper ${pythonEnv}/bin/python $out/bin/orchrd-tts \
              --add-flags "$out/libexec/orchrd/orchrd_tts.py" \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.pipewire ]}
            makeWrapper ${pythonEnv}/bin/pocket-tts $out/bin/pocket-tts

            extensionDir=$out/share/gnome-shell/extensions/orchrd@kacper.local
            mkdir -p "$extensionDir/schemas" "$extensionDir/icons"
            cp extension/extension.js extension/metadata.json extension/stylesheet.css "$extensionDir/"
            cp extension/schemas/*.xml "$extensionDir/schemas/"
            cp extension/icons/* "$extensionDir/icons/"
            glib-compile-schemas "$extensionDir/schemas"
            install -Dm644 dbus/io.github.kacper.Orchrd1.xml $out/share/dbus-1/interfaces/io.github.kacper.Orchrd1.xml
            install -Dm644 config.example.toml $out/share/doc/orchrd/config.example.toml
          '';
          meta = {
            description = "GNOME voice control for interactive coding agents";
            license = pkgs.lib.licenses.mit;
            platforms = pkgs.lib.platforms.linux;
            mainProgram = "orchrdctl";
          };
        };
      in
      {
        packages = {
          default = orchrd;
          inherit orchrd pocket-tts;
        };
        devShells.default = pkgs.mkShell {
          GI_TYPELIB_PATH = activityTypelibs;
          ORCHRD_TEST_PYTHON = "${activityPython}/bin/python";
          ORCHRD_TEST_GTK_DATA = "${pkgs.gtk4}/share";
          inputsFrom = [ orchrd ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            callgraphTool
            pkgs.graphviz
            pkgs.gjs
            pkgs.glib
            pkgs.dbus
            pkgs.sqlite
            pkgs.python3
            activityPython
            pkgs.gtk4
            pkgs.xvfb-run
          ];
          shellHook = ''
            export ORCHRD_REAL_CARGO="${pkgs.cargo}/bin/cargo"
            export ORCHRD_CALLGRAPH="${callgraphTool}/bin/cargo-callgraph"
            export ORCHRD_DOT="${pkgs.graphviz}/bin/dot"
            export ORCHRD_WORKTREE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
            export PATH="$ORCHRD_WORKTREE_ROOT/scripts/dev-bin:$PATH"
          '';
        };
        checks = {
          inherit orchrd;
          module-evaluation = import ./nix/module-checks.nix {
            inherit self nixpkgs home-manager pkgs system;
          };
        };
        formatter = pkgs.nixfmt;
      }
    );
}
