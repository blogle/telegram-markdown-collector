#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
tar -C "$root" --null -T <(while IFS= read -r -d '' file; do test -e "$root/$file" && printf '%s\0' "$file"; done < <(git -C "$root" ls-files --cached --others --exclude-standard -z)) -cf - | tar -C "$tmp" -xf -
for sibling in chadlands chatgpt_collector dotfiles ignis; do test ! -e "$tmp/../$sibling"; done
nix develop "$tmp" --command cargo fmt --check --manifest-path "$tmp/Cargo.toml"
nix develop "$tmp" --command cargo clippy --manifest-path "$tmp/Cargo.toml" --all-targets --all-features -- -D warnings
nix develop "$tmp" --command cargo test --manifest-path "$tmp/Cargo.toml" --all
