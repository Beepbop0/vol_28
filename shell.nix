{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    ffmpeg
    normalize
    cdrkit # Contains wodim
    sqlite
  ];

  # point to sqlite libraries + headers
  RUSTFLAGS = "-L${pkgs.lib.getLib pkgs.sqlite}/lib";
}
