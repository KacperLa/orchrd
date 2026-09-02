{
  description = "Orchrd voice control for Herdr coding agents";

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
          postInstall = ''
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
            description = "GNOME voice control for Herdr coding agents";
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
          ];
        };
        checks = { inherit orchrd; };
        formatter = pkgs.nixfmt;
      }
    );
}
