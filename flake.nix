{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      naersk,
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
        rad-portable-pkg = naersk-lib.buildPackage ./.;
        rad-portable-with-alias = pkgs.symlinkJoin {
          name = "rad-portable";
          paths = [ rad-portable-pkg ];
          postBuild = ''
            ln -s "$out/bin/rad-portable" "$out/bin/radp"
          '';
        };
      in
      {
        packages.default = rad-portable-with-alias;
        defaultPackage = rad-portable-with-alias;
        devShell =
          with pkgs;
          mkShell {
            buildInputs = [
              cargo
              rustc
              rust-analyzer
              rustfmt
              pre-commit
              rustPackages.clippy
            ];
            RUST_SRC_PATH = rustPlatform.rustLibSrc;
          };
      }
    );
}
