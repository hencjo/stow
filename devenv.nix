{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  packages = [
    pkgs.gh
    pkgs.git
  ];

  git-hooks.hooks = {
    rustfmt.enable = true;
    nixfmt.enable = true;
  };

  languages.rust = {
    enable = true;
    channel = "stable";
    targets = [ "x86_64-unknown-linux-musl" ];
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
    lsp.enable = true;
  };
}
