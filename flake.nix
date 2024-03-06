{
  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs = {
        flake-utils.follows = "flake-utils";
        nixpkgs.follows = "nixpkgs";
      };
    };
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };
  outputs = { self, crane, fenix, flake-utils, nixpkgs }:
    flake-utils.lib.eachDefaultSystem (system: {
      packages.onCrane =
        let
          craneLib = crane.lib.${system}.overrideToolchain
            fenix.packages.${system}.minimal.toolchain;
          pkgs = nixpkgs.legacyPackages.${system};
        in

        craneLib.buildPackage {
          src = ./.;
          buildInputs = with pkgs; [
            git
            pkg-config
            openssl
          ];
        };
      
      packages.default =
        let
          toolchain = fenix.packages.${system}.minimal.toolchain;
          pkgs = nixpkgs.legacyPackages.${system};
        in
        (pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        }).buildRustPackage {
          pname = "gluon_language-server";
          version = "0.18.1-alpha.0";

          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;
          cargoLock.outputHashes = {};
        };
      devShells.default = 
      let
          toolchain = fenix.packages.${system}.minimal.toolchain;
          pkgs = import nixpkgs { inherit system;
                                  overlays = [ fenix.overlays.default ];
                                };
      in
        pkgs.mkShell {
          nativeBuildInputs =
            with pkgs; [
              toolchain
              rust-analyzer-nightly
              
            ];
          
        };
    });
}
