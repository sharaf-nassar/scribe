#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./release.sh <command> [args]

Commands:
  bump <major|minor|patch>   Create and push a new version tag
  retag [version]            Replace an existing tag locally and remotely (defaults to latest)
  latest                     Show the latest version tag

Examples:
  ./release.sh bump patch        # v0.2.1 -> v0.2.2
  ./release.sh bump minor        # v0.2.1 -> v0.3.0
  ./release.sh bump major        # v0.2.1 -> v1.0.0
  ./release.sh retag 0.2.1       # Re-point v0.2.1 to current HEAD
  ./release.sh latest            # Print latest tag
EOF
  exit 1
}

get_latest_tag() {
  git tag --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n1 || true
}

parse_version() {
  local tag="$1"
  echo "${tag#v}"
}

bump_version() {
  local version="$1" part="$2"
  local major minor patch
  IFS='.' read -r major minor patch <<< "$version"

  case "$part" in
    major) echo "$((major + 1)).0.0" ;;
    minor) echo "${major}.$((minor + 1)).0" ;;
    patch) echo "${major}.${minor}.$((patch + 1))" ;;
    *) echo "Invalid part: $part" >&2; exit 1 ;;
  esac
}

generate_notes() {
  local prev_tag="$1" new_tag="$2"

  local range
  if [[ -z "$prev_tag" ]]; then
    range="HEAD"
  else
    range="${prev_tag}..HEAD"
  fi

  local commits diff_stat
  commits=$(git log "$range" --pretty=format:"- %s" --no-merges)
  diff_stat=$(git diff "${prev_tag:-$(git rev-list --max-parents=0 HEAD)}..HEAD" --stat)

  local prompt
  prompt=$(cat <<PROMPT
You are writing release notes for Scribe, a client-server terminal emulator with GPU rendering.

Focus ONLY on new features and capabilities that are visible to users.
For each feature, write a bold heading and 1-2 sentences describing what it does.
Write directly about the feature, not from the user's perspective — avoid "you can",
"your", "lets you". Example: "Workspace badges with deterministic colors"
not "You can now see colored workspace badges".

OMIT entirely: bug fixes, refactors, dependency updates, CI changes, internal
architecture changes, performance improvements, and anything not visible to users.
If a commit is purely technical with no visible impact, skip it.

Output format — a flat list under a single "## What's New" heading. No sub-sections.
If there are zero visible changes, output "Maintenance release — no user-facing changes."

Version: ${new_tag}
Previous version: ${prev_tag:-"(first release)"}

Commits:
${commits}

Files changed:
${diff_stat}
PROMPT
)

  echo "$prompt" | claude -p --model haiku --output-format text --no-session-persistence 2>/dev/null
}

cmd_bump() {
  local part="${1:-}"
  if [[ -z "$part" || ! "$part" =~ ^(major|minor|patch)$ ]]; then
    echo "Usage: ./release.sh bump <major|minor|patch>"
    exit 1
  fi

  local latest current new_version
  latest=$(get_latest_tag)
  if [[ -z "$latest" ]]; then
    current="0.0.0"
  else
    current=$(parse_version "$latest")
  fi

  new_version=$(bump_version "$current" "$part")
  echo "Current version: ${current}"
  echo "New version:     v${new_version}"
  echo ""

  echo "Generating release notes..."
  local notes
  notes=$(generate_notes "$latest" "v${new_version}")
  echo ""
  echo "--- Release Notes ---"
  echo "$notes"
  echo "---------------------"
  echo ""

  read -rp "Create and push tag v${new_version}? [Y/n] " confirm
  if [[ "$confirm" == [nN] ]]; then
    echo "Aborted."
    exit 0
  fi

  git tag -a "v${new_version}" -m "Release v${new_version}

${notes}"
  git push origin "v${new_version}"
  echo "Pushed v${new_version} - CI release workflow will start automatically."
}

cmd_retag() {
  local version="${1:-}"
  if [[ -z "$version" ]]; then
    local latest
    latest=$(get_latest_tag)
    if [[ -z "$latest" ]]; then
      echo "No version tags found."
      exit 1
    fi
    version=$(parse_version "$latest")
  fi

  # Strip v prefix if provided
  version="${version#v}"
  local tag="v${version}"

  if ! git tag -l "$tag" | grep -q .; then
    echo "Tag $tag does not exist locally."
    exit 1
  fi

  # Find the tag before this one for release notes range
  local prev_tag
  prev_tag=$(git tag --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | grep -v "^${tag}$" | head -n1)

  echo "This will re-point $tag to HEAD ($(git rev-parse --short HEAD))."
  echo "WARNING: This deletes the tag on the remote and re-pushes it."
  echo ""

  # Extract previous release notes from the existing tag annotation
  local prev_notes
  prev_notes=$(git tag -l --format='%(contents:body)' "$tag" | sed '/^$/d')

  local notes
  if [[ -n "$prev_notes" ]]; then
    echo "--- Previous Release Notes ---"
    echo "$prev_notes"
    echo "------------------------------"
    echo ""
    read -rp "Use previous release notes? [Y/n] " use_prev
    if [[ "$use_prev" == [nN] ]]; then
      echo ""
      echo "Generating new release notes..."
      notes=$(generate_notes "$prev_tag" "$tag")
      echo ""
      echo "--- New Release Notes ---"
      echo "$notes"
      echo "-------------------------"
    else
      notes="$prev_notes"
    fi
  else
    echo "No previous release notes found on $tag."
    echo ""
    echo "Generating release notes..."
    notes=$(generate_notes "$prev_tag" "$tag")
    echo ""
    echo "--- Release Notes ---"
    echo "$notes"
    echo "---------------------"
  fi
  echo ""

  read -rp "Continue? [Y/n] " confirm
  if [[ "$confirm" == [nN] ]]; then
    echo "Aborted."
    exit 0
  fi

  git tag -d "$tag"
  git tag -a "$tag" -m "Release ${tag}

${notes}"
  git push origin ":refs/tags/$tag"
  git push origin "$tag"
  echo "Re-tagged $tag to $(git rev-parse --short HEAD) locally and remotely."
}

cmd_latest() {
  local latest
  latest=$(get_latest_tag)
  if [[ -z "$latest" ]]; then
    echo "No version tags found."
  else
    echo "$latest ($(parse_version "$latest"))"
  fi
}

[[ $# -lt 1 ]] && usage

command="$1"
shift

case "$command" in
  bump)   cmd_bump "$@" ;;
  retag)  cmd_retag "$@" ;;
  latest) cmd_latest "$@" ;;
  *)      usage ;;
esac
