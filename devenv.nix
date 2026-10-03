{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  # https://devenv.sh/languages/
  languages.rust.enable = true;

  # mkfs.btrfs and mkfs.xfs for scripts/testvol.sh
  packages = with pkgs; [
    btrfs-progs
    xfsprogs
  ];

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
