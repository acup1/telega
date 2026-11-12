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
          mpvBase = pkgs.mpv-unwrapped.override {
            archiveSupport = false;
            bluraySupport = false;
            cacaSupport = false;
            cmsSupport = false;
            curlSupport = false;
            dvdnavSupport = false;
            javascriptSupport = false;
            openalSupport = false;
            rubberbandSupport = false;
            vdpauSupport = false;
            vulkanSupport = false;
            zimgSupport = false;
          };
          terminalMpv = if pkgs.stdenv.hostPlatform.isDarwin then
            mpvBase.overrideAttrs (old: {
              nativeBuildInputs = builtins.filter
                (input: input != pkgs.swift)
                old.nativeBuildInputs;
              mesonFlags = old.mesonFlags ++ [
                "-Dcocoa=disabled"
                "-Dswift-build=disabled"
                "-Dcoreaudio=enabled"
                "-Davfoundation=disabled"
              ];
              postBuild = "";
              postInstall = ''
                mkdir -p $out/share/mpv
                ln -s ${pkgs.freefont_ttf}/share/fonts/truetype/FreeSans.ttf \
                  $out/share/mpv/subfont.ttf
                cp ../TOOLS/mpv_identify.sh ../TOOLS/umpv $out/bin/
              '';
            })
          else mpvBase;
          runtime = with pkgs; [
            ffmpeg
            librsvg
            terminalMpv
            (python3.withPackages (ps: [ ps.lottie ]))
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            pkgs.wl-clipboard
            pkgs.xclip
          ];
          unwrapped = pkgs.rustPlatform.buildRustPackage {
            pname = "telega";
            version = "0.1.0";
            TG_API_ID = credentials.TG_API_ID or "";
            TG_API_HASH = credentials.TG_API_HASH or "";
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
              description = "Keyboard-first Telegram TUI with Kitty graphics media";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "telega";
              platforms = systems;
            };
          };
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
          package = pkgs.symlinkJoin {
            name = "telega-0.1.0";
            paths = [ unwrapped ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = ''
              wrapProgram $out/bin/telega \
                --set-default FONTCONFIG_FILE ${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }} \
                --prefix PATH : ${pkgs.lib.makeBinPath runtime}
            '';
            inherit (unwrapped) meta;
          };
        in {
          default = package;
          telega = package;
          telega-unwrapped = unwrapped;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/telega";
        };
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
