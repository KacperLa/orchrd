{
  description = "Apple TV remote Bluetooth touchpad and microphone services for NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, ... }: {
    nixosModules.default = import ./apple-remote.nix;
    nixosModules.apple-tv-remote = self.nixosModules.default;
  };
}
