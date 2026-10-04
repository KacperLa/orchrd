{
  description = "Orchrd voice control for interactive coding agents";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
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
        bridgePython = pkgs.python3.withPackages (p: [ p.websockets ]);
        activityTypelibs = pkgs.lib.makeSearchPath "lib/girepository-1.0" (map pkgs.lib.getLib [ pkgs.gtk4 pkgs.pango pkgs.harfbuzz pkgs.gdk-pixbuf pkgs.graphene pkgs.glib pkgs.gobject-introspection ]);
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
            python3 -m unittest discover -s tts
            ${bridgePython}/bin/python -m unittest discover -s adapters
            env -u GIO_EXTRA_MODULES -u GIO_MODULE_DIR GI_TYPELIB_PATH="${activityTypelibs}" GDK_BACKEND=x11 \
              XDG_DATA_DIRS="${pkgs.gtk4}/share" GTK_A11Y=none GIO_USE_VFS=local GTK_USE_PORTAL=0 \
              xvfb-run -a dbus-run-session --config-file=ui/test-bus.conf -- ${activityPython}/bin/python -m unittest discover -s ui
          '';
          postInstall = ''
            wrapProgram $out/bin/orchrdd \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.pipewire ]}
            install -d $out/libexec/orchrd
            install -m755 adapters/codex_bridge.py $out/libexec/orchrd/codex_bridge.py
            makeWrapper ${bridgePython}/bin/python $out/bin/orchrd-codex \
              --add-flags "$out/libexec/orchrd/codex_bridge.py"
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
          ORCHRD_BRIDGE_PYTHON = "${bridgePython}/bin/python";
          ORCHRD_TEST_GTK_DATA = "${pkgs.gtk4}/share";
          inputsFrom = [ orchrd ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            pkgs.gjs
            pkgs.glib
            pkgs.dbus
            pkgs.sqlite
            pkgs.python3
            activityPython
            pkgs.gtk4
            pkgs.xvfb-run
          ];
        };
        checks = { inherit orchrd; };
        formatter = pkgs.nixfmt;
      }
    );
}
