{
  description = "telega release with media support";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      release = builtins.fromJSON (builtins.readFile ./release.json);
      pname = release.program;
    in {
      packages = nixpkgs.lib.genAttrs release.systems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          unwrapped = pkgs.stdenv.mkDerivation {
            inherit pname;
            version = release.tag;
            src = ./. + "/${pname}-${system}.tar.gz";
            dontUnpack = true;
            dontStrip = true;
            nativeBuildInputs = pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.autoPatchelfHook;
            buildInputs = pkgs.lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.stdenv.cc.cc.lib;
            installPhase = ''
              mkdir -p "$out"
              tar -xzf "$src" -C "$out" bin/${pname} LICENSE
            '';
            doInstallCheck = true;
            installCheckPhase = ''"$out/bin/${pname}" --help > /dev/null'';
            meta = {
              mainProgram = pname;
              license = pkgs.lib.licenses.gpl3Only;
              platforms = release.systems;
            };
          };
        in { default = import ./wrap.nix { inherit pkgs unwrapped; }; });
    };
}
