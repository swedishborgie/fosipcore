{
  description = "fosipcore — WebSocket-to-RTSP proxy for legacy Foscam IP camera web UIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    {
      # `nix fmt` for the flake files.
      formatter.x86_64-linux = nixpkgs.legacyPackages.x86_64-linux.nixfmt-rfc-style;
      formatter.aarch64-linux = nixpkgs.legacyPackages.aarch64-linux.nixfmt-rfc-style;

      # NixOS module: `services.fosipcore.enable + users.<name>.enable`
      # wires up the package's fosipcore@.service template unit (see
      # nixos/fosipcore.nix for details and usage).
      nixosModules.default = import ./nixos/fosipcore.nix;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.fosipcore = import ./pkgs/nix/fosipcore.nix pkgs;
        defaultPackage = self.packages.${system}.fosipcore;
      }
    );
}
