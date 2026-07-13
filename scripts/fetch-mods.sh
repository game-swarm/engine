#!/bin/bash
# Fetch mod sources for engine build.
# Reads mods.toml and clones/fetches each mod into engine/mods/.
#
# Usage:
#   ./scripts/fetch-mods.sh              # Fetch all mods from git (default)
#   ./scripts/fetch-mods.sh --local      # Prefer local paths from mods.toml
#   MOD_REV=develop ./scripts/fetch-mods.sh  # Override revision for all

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENGINE_DIR="$(dirname "$SCRIPT_DIR")"
CONFIG="$ENGINE_DIR/mods.toml"
MODS_DIR="$ENGINE_DIR/mods"
USE_LOCAL="${USE_LOCAL:-false}"
MOD_REV="${MOD_REV:-}"

# Check for --local flag
if [ "${1:-}" = "--local" ]; then
  USE_LOCAL=true
fi

mkdir -p "$MODS_DIR"

# Simple TOML parser for mods.toml
# Handles: name = { git = "url", rev = "branch" }
#          name = { path = "/path" }
parse_mods() {
  local current_name=""
  local current_git=""
  local current_rev="main"
  local current_path=""
  
  while IFS= read -r line; do
    # Strip inline comments (but not # inside strings)
    line="${line%%#*}"

    # Match mod entries: name = { ... }
    if [[ "$line" =~ ^[[:space:]]*([a-zA-Z0-9_-]+)[[:space:]]*=[[:space:]]*\{ ]]; then
      current_name="${BASH_REMATCH[1]}"
      current_git=""
      current_rev="main"
      current_path=""
    fi

    # Extract key = "value" pairs inside the braces
    if [[ "$line" =~ git[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
      current_git="${BASH_REMATCH[1]}"
    fi
    if [[ "$line" =~ rev[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
      current_rev="${BASH_REMATCH[1]}"
    fi
    if [[ "$line" =~ path[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
      current_path="${BASH_REMATCH[1]}"
    fi

    # When we close the brace, emit the mod if we have a name
    if [[ "$line" == *"}"* ]] && [ -n "$current_name" ]; then
      if [ -n "$current_path" ] && [ "$USE_LOCAL" = "true" ]; then
        echo "PATH|$current_name|$current_path"
      elif [ -n "$current_git" ]; then
        local rev="$current_rev"
        [ -n "$MOD_REV" ] && rev="$MOD_REV"
        echo "GIT|$current_name|$current_git|$rev"
      fi
      current_name=""
    fi
  done < "$CONFIG"
}

# Process each mod
parse_mods | while IFS='|' read -r type name url rev; do
  target="$MODS_DIR/$name"

  case "$type" in
    PATH)
      if [ -d "$url" ]; then
        echo "[mod] $name: symlink from $url"
        ln -sfn "$url" "$target"
      else
        echo "[mod] WARNING: $name: local path $url not found, skipping" >&2
      fi
      ;;

    GIT)
      rev="${rev:-main}"
      if [ -d "$target/.git" ]; then
        echo "[mod] $name: updating ($rev)..."
        git -C "$target" fetch --depth 1 origin "$rev"
        git -C "$target" checkout --detach -q FETCH_HEAD
      else
        echo "[mod] $name: fetching ($rev)..."
        git init -q "$target"
        git -C "$target" remote add origin "$url"
        git -C "$target" fetch --depth 1 origin "$rev"
        git -C "$target" checkout --detach -q FETCH_HEAD
      fi
      ;;

    *)
      echo "[mod] WARNING: unknown mod type for $name" >&2
      ;;
  esac
done

echo "[mod] All mods fetched."
