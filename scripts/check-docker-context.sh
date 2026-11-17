#!/usr/bin/env bash
# The deploy builds the server from only the paths the Dockerfile copies. A file the code embeds
# (include_str!, include_bytes!, sqlx::migrate!, rust-embed) from anywhere else compiles here but
# breaks the deploy. This checks the server compiles from exactly those paths.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT

# COPY lines of the `server` build stage that copy from the repository (not from another stage).
awk '/^FROM /{in_server = ($0 ~ / AS server$/)} in_server && /^COPY / && $2 !~ /^--from/' Dockerfile |
while read -r _ args; do
  read -ra parts <<<"$args"
  dest=${parts[-1]}
  unset 'parts[-1]'
  for pattern in "${parts[@]}"; do
    for src in $pattern; do   # unquoted on purpose: Dockerfile sources may be globs
      [ -e "$src" ] || continue
      if [ -d "$src" ]; then
        mkdir -p "$ctx/$dest"
        cp -R "$src/." "$ctx/$dest/"
      else
        mkdir -p "$ctx/$dest"
        cp "$src" "$ctx/$dest/"
      fi
    done
  done
done

# The image builds the dashboard in its own stage; a placeholder is enough to compile against.
mkdir -p "$ctx/web/dist"
cp web/dist/.placeholder.html "$ctx/web/dist/"

cd "$ctx"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}" cargo check --locked
echo "The server compiles from the paths the Dockerfile copies."
