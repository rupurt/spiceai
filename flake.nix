{
  description = "Spice. A portable runtime offering developers a unified SQL interface to materialize, accelerate, and query data";

  inputs = {
    dream2nix.url = "github:nix-community/dream2nix";
    nixpkgs.follows = "dream2nix/nixpkgs";
  };

  outputs = {
    self,
    dream2nix,
    nixpkgs,
    ...
  }: let
    supportedSystems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];
    forEachSupportedSystem = f:
      nixpkgs.lib.genAttrs supportedSystems (supportedSystem:
        f {
          system = supportedSystem;
          pkgs = dream2nix.inputs.nixpkgs.legacyPackages.${supportedSystem};
        });
  in {
    packages = forEachSupportedSystem ({pkgs, ...}: rec {
      spice = pkgs.buildGoModule {
        name = "spice";
        src = ./.;
        vendorHash = "sha256-9bjpd73NyNVfOFgoad27MElgTLZl2P//iUdwUn70ypc=";
        runVend = true;
      };
      spiced = dream2nix.lib.evalModules {
        packageSets.nixpkgs = pkgs;
        modules = [
          ./spiced.nix
          {
            paths.projectRoot = ./.;
            paths.projectRootFile = "flake.nix";
            paths.package = ./.;
          }
        ];
      };
      default = spice;
    });

    formatter = forEachSupportedSystem ({pkgs, ...}: pkgs.alejandra);

    devShells = forEachSupportedSystem ({
      system,
      pkgs,
      ...
    }: {
      default = pkgs.mkShell {
        inputsFrom = [
          self.packages.${system}.default.devShell
        ];

        packages = with pkgs; [
          go
          gopls
        ];
      };
    });

    overlay = final: prev: {
      spice-pkgs = self.packages.${prev.system};
    };
  };
}
