{
  inputs = {
    nixpkgs.url      = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
    naersk.url       = "github:nix-community/naersk";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, naersk }:
    flake-utils.lib.eachDefaultSystem (system:
      let 
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust = (pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "cargo"
            "rustc"
            "rustfmt"
          ];
        });
        naersk-lib = naersk.lib."${system}".override {
          cargo = rust;
          rustc = rust;
        };
      in rec {
        packages.r9ktg = naersk-lib.buildPackage {
          pname = "r9ktg";
          root = ./.;
        };
        defaultPackage = packages.r9ktg;

        apps.r9ktg = packages.r9ktg;
        defaultApp = apps.r9ktg;

        nixosModules.default = with pkgs.lib; { config, ... }:
        let cfg = config.services.r9ktg;
        in {
          options.services.r9ktg = {
            enable = mkEnableOption "Robot9000 for Telegram";
            envFile = mkOption {
              type = types.str;
              default = "/etc/r9ktg.env";
            };
          };
          config = mkIf cfg.enable {
            systemd.services.r9ktg = {
              wantedBy = [ "multi-user.target" ];
              serviceConfig.ExecStart = "${self.defaultPackage.${system}}/bin/r9ktg";
              serviceConfig.EnvironmentFile = cfg.envFile;
            };
          };
        };

        devShell = pkgs.mkShell {
          buildInputs = [
            rust
            pkgs.rust-analyzer
          ];
        };
      }
    );
}
