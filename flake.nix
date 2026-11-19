{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    gitignore = {
      url = "github:hercules-ci/gitignore.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  nixConfig = {
  };

  outputs = {
    self,
    flake-utils,
    nixpkgs,
    crane,
    fenix,
    gitignore,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            fenix.overlays.default
          ];
        };

        toolchain = pkgs.fenix.complete.toolchain;

        craneLib = (crane.mkLib pkgs).overrideToolchain (p: toolchain);

        src = gitignore.lib.gitignoreSource ./.;

        runtimeDeps = with pkgs; [
          ffmpeg
          bubblewrap
        ];
      in {
        devShell = craneLib.devShell {
          checks = self.checks.${system};

          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.llvmPackages.clangUseLLVM}/bin/clang";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=--ld-path=${pkgs.mold}/bin/mold";

          RUST_LOG = "warn,thamus=trace";

          packages = with pkgs; [
            nixd
            sqlite
          ];
        };

        checks = {
          thamus = self.packages.${system}.thamus;
        };

        packages.default = self.packages.${system}.thamus;
        packages.thamus = craneLib.buildPackage {
          inherit src;

          strictDeps = true;

          buildInputs = with pkgs; [
            openssl

            libopus
            opusfile
          ];
          nativeBuildInputs = with pkgs; [
            libgcc

            pkg-config
            cmake

            makeWrapper
          ];

          postInstall = ''
            wrapProgram $out/bin/thamus --prefix LD_LIBRARY_PATH : ${nixpkgs.lib.makeLibraryPath runtimeDeps}
          '';
        };
      }
    );
}
