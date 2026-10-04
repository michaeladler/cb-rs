{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  overlays = [ inputs.michaeladler-nurpackages.overlays.default ];

  # https://devenv.sh/languages/
  languages.rust.enable = true;

  packages = [
    pkgs.btrfs-progs
    pkgs.xfsprogs

    pkgs.demo-magic
    pkgs.asciinema
    pkgs.agg
    pkgs.tmux
  ];

  # https://devenv.sh/git-hooks/
  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
    prettier.enable = true;
  };

  # See full reference at https://devenv.sh/reference/options/
}
