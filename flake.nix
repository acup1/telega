{
  description = "Terminal Telegram client built on terminalib";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          mkUnwrapped = pname: pkgs.rustPlatform.buildRustPackage ({
            inherit pname;
            version = "0.1.0";
            cargoBuildFlags = [ "-p" pname ];
            cargoTestFlags = [ "-p" pname ];
            src = builtins.path {
              name = "telega-source";
              path = ./.;
              filter = path: type:
                let
                  relative = pkgs.lib.removePrefix
                    (toString ./. + "/")
                    (toString path);
                in relative == "Cargo.toml"
                  || relative == "build.rs"
                  || relative == "Cargo.lock"
                  || relative == "crates"
                  || relative == "crates/tgp-viewer"
                  || pkgs.lib.hasPrefix "crates/tgp-viewer/" relative
                  || relative == "tests"
                  || pkgs.lib.hasPrefix "tests/" relative
                  || relative == "src"
                  || pkgs.lib.hasPrefix "src/" relative;
            };
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                "terminalib-0.1.0" = "sha256-CTHQiSxSgi32gGMeuYtkqerMAbjypoHVGcAyw/8wUU8=";
              };
            };
            nativeBuildInputs = with pkgs; [
              cmake
              pkg-config
              rustPlatform.bindgenHook
            ];
            doCheck = true;
            meta = {
              description = if pname == "telega" then "Telegram client for the terminal"
                else "Terminal image and video viewer with Kitty graphics";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = pname;
              platforms = systems;
            };
          } // pkgs.lib.optionalAttrs (pname == "telega") {
            TG_API_ID = credentials.TG_API_ID or "";
            TG_API_HASH = credentials.TG_API_HASH or "";
          });
          credentials = if builtins.pathExists ./api-credentials.env then
            builtins.listToAttrs (builtins.filter (entry: entry != null)
              (map (line:
                let parts = pkgs.lib.splitString "=" line;
                in if builtins.length parts == 2
                  && builtins.elem (builtins.head parts) [ "TG_API_ID" "TG_API_HASH" ]
                  then { name = builtins.head parts; value = builtins.elemAt parts 1; }
                  else null)
                (pkgs.lib.splitString "\n" (builtins.readFile ./api-credentials.env))))
            else {};
          wrap = unwrapped: import ./nix/wrap.nix { inherit pkgs unwrapped; };
          telega = mkUnwrapped "telega";
        in {
          default = wrap telega;
          telega = wrap telega;
          telega-unwrapped = telega;
          tgp-viewer = wrap (mkUnwrapped "tgp-viewer");
        });

      devShells = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              cmake
              pkg-config
              rustPlatform.bindgenHook
              ffmpeg
              librsvg
              (python3.withPackages (ps: [ ps.lottie ]))
            ];
          };
        });
    };
}
