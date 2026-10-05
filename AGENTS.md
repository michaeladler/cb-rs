## Build and test through the flake, not cargo

Do not run `cargo build`, `cargo test`, or `cargo clippy` for verification. The dev shell
hydrates `target/` with Nix-built dependencies, so a local cargo build still recompiles the
whole dependency tree, and `cargo test` is minutes of setup for two seconds of tests. The
flake checks build exactly what CI builds, and reuse the store between runs.

Run everything:

```sh
nix flake check
```

One check at a time, with the logs of the failing one:

```sh
nix build .#checks.x86_64-linux.my-crate-nextest --no-link --print-build-logs
```
