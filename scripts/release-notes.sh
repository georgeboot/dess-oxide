#!/usr/bin/env bash
# Prints the release notes for a version: its section of the app's changelog,
# and where the image is. Used by the release workflow.
set -euo pipefail
version="${1:?usage: scripts/release-notes.sh <version>}"
notes=$(awk -v heading="## $version" '
  $0 == heading { on = 1; next }
  on && /^## / { exit }
  on { print }
' dess_oxide/CHANGELOG.md | sed -e '/./,$!d')
if [ -z "$notes" ]; then
  echo "no changelog section for $version" >&2
  exit 1
fi
printf '%s\n\nHome Assistant app image: `ghcr.io/georgeboot/dess-oxide:%s` (amd64, aarch64).\n' "$notes" "$version"
