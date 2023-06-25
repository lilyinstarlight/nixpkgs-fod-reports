{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    nix-filter.url = "github:numtide/nix-filter";
  };

  outputs = inputs@{ flake-parts, ... }: flake-parts.lib.mkFlake { inherit inputs; } ({ lib, inputs, ... }: {
    systems = lib.intersectLists (lib.platforms.x86_64 ++ lib.platforms.aarch64) (lib.platforms.linux ++ lib.platforms.darwin);
    perSystem = { pkgs, self', ... }: {
      packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "nixpkgs-fod-reports";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

          src = inputs.nix-filter.lib.filter {
            root = ./.;
            exclude = [ ".github" "flake.nix" "flake.lock" ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.rustfmt pkgs.clippy ];

          preCheck = ''
            cargo fmt --check
            cargo clippy
          '';
      };
      checks = { inherit (self'.packages) default; };
      devShells = {
        inherit (self'.packages) default;
        run = pkgs.mkShellNoCC {
          packages = [ self'.packages.default ];
          shellHook = ''
            exec nixpkgs_fod_reports ${pkgs.path}
          '';
        };
      };
    };
  });
}
