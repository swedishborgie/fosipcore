{
  description = "fosipcore — WebSocket-to-RTSP proxy for legacy Foscam IP camera web UIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      # `nix fmt` for the flake files.
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt-rfc-style);

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          fosipcore = pkgs.callPackage ./pkgs/nix/fosipcore.nix { };
          default = self.packages.${system}.fosipcore;
        }
      );

      # NixOS module: `services.fosipcore.enable + users.<name>.enable`
      # wires up the package's fosipcore@.service template unit (see
      # nixos/fosipcore.nix for details and usage). The module builds the
      # package with the *consuming system's* nixpkgs, so the nixpkgs input
      # above only matters for the standalone outputs (packages, checks,
      # formatter) — consumers should `follows` their own nixpkgs.
      nixosModules = {
        fosipcore = import ./nixos/fosipcore.nix;
        default = self.nixosModules.fosipcore;
      };

      checks = forAllSystems (system: {
        fosipcore = self.packages.${system}.fosipcore;
      });
    };
}
