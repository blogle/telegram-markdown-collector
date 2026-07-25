#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pattern='chadlands|mud[-_ ]room|reckoning|gigachad|strategy[-_ ]session|scoped_workspace|/home/ogle|\.\./(dotfiles|chatgpt_collector|ignis)'
mapfile -t files < <(git -C "$root" ls-files 'src/**' 'fixtures/**' 'config.example.yaml' 'README.md' 'DESIGN.md' 'CREDENTIALS.md')
if ((${#files[@]})) && git -C "$root" grep -nEi "$pattern" -- "${files[@]}"; then
  echo 'collector boundary audit failed' >&2
  exit 1
fi
echo "collector boundary audit passed (${#files[@]} tracked files)"
