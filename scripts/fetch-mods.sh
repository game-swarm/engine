#!/bin/bash
# Fetch mod sources for an engine build.
# Cargo resolves the engine API and plugin SDK directly from their Git release tag.
# The sibling mod layout matches the path dependencies in Cargo.toml:
#   ../mods/<mod-name>
#
# Usage:
#   ./scripts/fetch-mods.sh              # Fetch missing mod repositories
#   ./scripts/fetch-mods.sh --local      # Prefer local mod paths from mods.toml
#   ALLOW_MUTABLE_REFS=true MOD_REV=develop ./scripts/fetch-mods.sh
#       # Explicitly opt in to mutable revisions for coordinated development

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENGINE_DIR="$(dirname "$SCRIPT_DIR")"
CONFIG="$ENGINE_DIR/mods.toml"
WORKSPACE_DIR="$(dirname "$ENGINE_DIR")"
MODS_DIR="$WORKSPACE_DIR/mods"
USE_LOCAL="${USE_LOCAL:-false}"
MOD_REV="${MOD_REV:-}"
ALLOW_MUTABLE_REFS="${ALLOW_MUTABLE_REFS:-false}"

# Check for --local flag
if [ "${1:-}" = "--local" ]; then
  USE_LOCAL=true
fi

mkdir -p "$MODS_DIR"

is_immutable_revision() {
  [[ "$1" =~ ^[0-9a-fA-F]{40}$ ]]
}

require_immutable_revision() {
  local name="$1"
  local rev="$2"

  if is_immutable_revision "$rev"; then
    return
  fi
  if [ "$ALLOW_MUTABLE_REFS" = "true" ]; then
    echo "[$name] WARNING: mutable revision explicitly allowed: $rev" >&2
    return
  fi
  echo "[$name] ERROR: revision must be a full 40-character commit SHA: $rev" >&2
  echo "[$name] Set ALLOW_MUTABLE_REFS=true only for coordinated development." >&2
  return 1
}

normalize_git_url() {
  local url="${1%/}"
  echo "${url%.git}"
}

fetch_repository() {
  local name="$1"
  local target="$2"
  local url="$3"
  local rev="$4"

  require_immutable_revision "$name" "$rev"

  if [ -d "$target/.git" ]; then
    if [ "$USE_LOCAL" = "true" ]; then
      echo "[$name] using local checkout at $target"
      return
    fi
    local actual_url
    actual_url="$(git -C "$target" remote get-url origin)"
    if [ "$(normalize_git_url "$actual_url")" != "$(normalize_git_url "$url")" ]; then
      echo "[$name] ERROR: origin mismatch for $target" >&2
      echo "[$name] expected: $url" >&2
      echo "[$name] actual:   $actual_url" >&2
      return 1
    fi
    echo "[$name] updating ($rev)..."
    git -C "$target" fetch --depth 1 origin "$rev"
  elif [ -e "$target" ]; then
    if [ "$USE_LOCAL" = "true" ]; then
      echo "[$name] using local source tree at $target"
      return
    fi
    echo "[$name] ERROR: $target exists but is not a Git checkout" >&2
    return 1
  else
    echo "[$name] fetching ($rev)..."
    git init -q "$target"
    git -C "$target" remote add origin "$url"
    git -C "$target" fetch --depth 1 origin "$rev"
  fi

  git -C "$target" checkout --detach -q FETCH_HEAD
  if is_immutable_revision "$rev"; then
    local actual_rev
    actual_rev="$(git -C "$target" rev-parse HEAD)"
    if [ "${actual_rev,,}" != "${rev,,}" ]; then
      echo "[$name] ERROR: fetched revision mismatch" >&2
      echo "[$name] expected: $rev" >&2
      echo "[$name] actual:   $actual_rev" >&2
      return 1
    fi
  fi
}

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
      fetch_repository "mod:$name" "$target" "$url" "$rev"
      ;;

    *)
      echo "[mod] WARNING: unknown mod type for $name" >&2
      ;;
  esac
done

echo "[mods] Mod sources are ready in $MODS_DIR."
