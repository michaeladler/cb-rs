{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  overlays = [ inputs.michaeladler-nurpackages.overlays.default ];

  profiles = {
    dev.module = {
      languages.rust.enable = true;
    };

    demo.module = {
      packages = [
        pkgs.demo-magic
        pkgs.asciinema
        pkgs.agg
        pkgs.tmux
      ];
    };
    test.module = {
      packages = [
        pkgs.btrfs-progs
        pkgs.xfsprogs
      ];
    };
    release.module = {
      packages = [
        pkgs.goreleaser
        pkgs.cargo-zigbuild
        pkgs.zig
      ];
    };
  };

  # https://devenv.sh/git-hooks/
  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
    prettier.enable = true;
  };

  # See full reference at https://devenv.sh/reference/options/
}
