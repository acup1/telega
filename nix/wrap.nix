{ pkgs, unwrapped }:

let
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
  program = unwrapped.meta.mainProgram;
  runtime = with pkgs; [
    ffmpeg
    librsvg
    terminalMpv
  ] ++ pkgs.lib.optionals (program == "telega") [
    (python3.withPackages (ps: [ ps.lottie ]))
  ] ++ pkgs.lib.optionals (program == "telega" && pkgs.stdenv.hostPlatform.isLinux) [
    pkgs.wl-clipboard
    pkgs.xclip
  ];
in
pkgs.symlinkJoin {
  name = unwrapped.name;
  paths = [ unwrapped ];
  nativeBuildInputs = [ pkgs.makeWrapper ];
  postBuild = ''
    wrapProgram "$out/bin/${program}" \
      --set-default FONTCONFIG_FILE ${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }} \
      --prefix PATH : ${pkgs.lib.makeBinPath runtime}
  '';
  inherit (unwrapped) meta;
}
