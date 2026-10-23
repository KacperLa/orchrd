{
  description = "Apple TV Siri Remote Bluetooth touchpad and microphone services for NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      # Bindgen generates the PipeWire bindings and needs libclang; the
      # audiopus_sys build script finds libopus through pkg-config.
      nativeBuildInputs = pkgs: [
        pkgs.pkg-config
        pkgs.llvmPackages.clang
        pkgs.llvmPackages.libclang
      ];
      buildInputs = pkgs: [
        pkgs.libopus
        pkgs.pipewire
      ];
      source = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          let
            name = baseNameOf (toString path);
          in
          lib.cleanSourceFilter path type
          && !(builtins.elem name [
            "target"
            "result"
            ".direnv"
            "__pycache__"
          ])
          && !(lib.hasSuffix ".pyc" name);
      };
      package =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "apple-tv-remote";
          version = "0.2.0";
          src = source;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = nativeBuildInputs pkgs;
          buildInputs = buildInputs pkgs;
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          # The PipeWire session test starts its own server, which the sandbox
          # cannot do; it runs from `nix develop` instead. `--skip` matches test
          # function names, not the test binary or file name.
          cargoTestFlags = [
            "--lib"
            "--bins"
            "--tests"
            "--"
            "--skip"
            "microphone_source_is_published_selected_and_consumed"
          ];
          meta = {
            description = "Apple TV Siri Remote touchpad and Opus microphone daemon";
            license = lib.licenses.mit;
            platforms = lib.platforms.linux;
            mainProgram = "apple-tv-remote";
          };
        };
      forSystem =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          remote = package pkgs;
        in
        {
          packages = {
            default = remote;
            inherit remote;
          };
          formatter = pkgs.nixfmt;
          checks = {
            inherit remote;
            module-evaluation = import ./nix/module-checks.nix { inherit pkgs remote; };
          };
          devShells.default = pkgs.mkShell {
            # Everything needed to build, test and run the daemon, including a
            # private PipeWire session for the integration test.
            packages = (nativeBuildInputs pkgs) ++ [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.cmake
              pkgs.pipewire
              pkgs.wireplumber
              pkgs.dbus
              pkgs.bluez
            ];
            buildInputs = buildInputs pkgs;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            PKG_CONFIG_PATH = lib.makeSearchPathOutput "dev" "lib/pkgconfig" (buildInputs pkgs);
            shellHook = ''
              echo "Apple TV remote: 'cargo test' runs the unit tests;"
              echo "  'dbus-run-session -- cargo test --test pipewire_session' runs the audio integration test."
            '';
          };
        };
      perSystem = lib.genAttrs systems forSystem;
      fromSystems = attribute: lib.mapAttrs (_: outputs: outputs.${attribute}) perSystem;
    in
    {
      packages = fromSystems "packages";
      devShells = fromSystems "devShells";
      checks = fromSystems "checks";
      formatter = fromSystems "formatter";
      # The module resolves the daemon from this flake's packages, so a host
      # never has to pass a package explicitly.
      nixosModules.default = import ./nix/module.nix { inherit self; };
      nixosModules.apple-tv-remote = self.nixosModules.default;
    };
}
