{
  description = "Apple TV Siri Remote Bluetooth connection setup for NixOS";

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
          version = "0.1.0";
          src = source;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "Apple TV Siri Remote Bluetooth connection setup daemon";
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
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.bluez
            ];
            shellHook = ''
              echo "Apple TV remote: 'cargo test' runs the unit tests."
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
