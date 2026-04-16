alias b := build
alias c := check
alias t := test

[private]
default:
  @just --list

build *ARGS="--workspace --all-targets":
  cargo build {{ARGS}}

check *ARGS="--workspace --all-targets":
  cargo check {{ARGS}}

test:
  cargo test --workspace

format:
  cargo fmt --all

clippy *ARGS="--workspace --all-targets -- -D warnings":
  cargo clippy {{ARGS}}

final-check: clippy
  cargo fmt --all -- --check
  cargo test --workspace
