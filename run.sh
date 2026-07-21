#!/usr/bin/env sh
# Build and run autochord natively on the host.
#
# Needs Rust, a real terminal, and an audio device. (The Docker path was
# dropped: a Linux container can't produce a macOS audio binary, nor reach
# CoreAudio to make sound.)
#
#   ./run.sh              build + run (release)
#   ./run.sh --debug      build + run (debug, faster to compile)
#   ./run.sh --et         disable just intonation (plain 12-TET)
# (--debug is consumed here; any other flags pass through to the app.)
set -e
cd "$(dirname "$0")"

mode=release
if [ "$1" = "--debug" ]; then
  mode=debug
  shift
fi

if ! command -v cargo >/dev/null 2>&1; then
  cat >&2 <<'EOF'
cargo not found. Install Rust (user-local, no sudo, removable with
`rustup self uninstall`):

  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  . "$HOME/.cargo/env"

EOF
  exit 1
fi

if [ "$mode" = debug ]; then
  cargo run -- "$@"
else
  cargo build --release
  echo "binary: $(pwd)/target/release/autochord"
  exec ./target/release/autochord "$@"
fi
