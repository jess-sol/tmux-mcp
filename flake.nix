{
  # The toolchain is pinned in ./rust-toolchain.toml, which rustup reads too.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, rust-overlay, ... }:
    let
      # A packager of this flake evaluates for every system it supports, not
      # just the one this repo is developed on.
      forAllSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-darwin" ];
      pkgsFor = system: import nixpkgs { inherit system; overlays = [ (import rust-overlay) ]; };
      rustFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rust = rustFor pkgs;
        in
        {
          default = (pkgs.makeRustPlatform { cargo = rust; rustc = rust; }).buildRustPackage {
            pname = "tmux-mcp";
            inherit ((nixpkgs.lib.importTOML ./Cargo.toml).package) version;
            src = self;
            # Crates are vendored from the lockfile in this source, so a
            # dependency bump never costs a hash anywhere downstream.
            cargoLock.lockFile = ./Cargo.lock;
            # The unit suite only: the integration tests drive a live tmux
            # server and daemon, which the build sandbox does not host.
            cargoTestFlags = [ "--lib" "--bins" ];
          };
        });

      devShells = forAllSystems (system:
        let pkgs = pkgsFor system; in
        {
          default = pkgs.mkShell {
            buildInputs = [
              (rustFor pkgs)
              pkgs.openssl
              pkgs.pkg-config
              # The integration suite runs each subject inside its own tmux server.
              pkgs.tmux
              # Its subject shells are `bash -i` from PATH, which mkShell's
              # stdenv would supply without readline.
              pkgs.bashInteractive
              # Runs .pre-commit-config.yaml. `prek install` once per clone.
              pkgs.prek
              # The licence gate in .pre-commit-config.yaml.
              pkgs.cargo-deny
              # Cuts a release as release.toml sets it up.
              pkgs.cargo-release
            ];
          };
        });
    };
}
