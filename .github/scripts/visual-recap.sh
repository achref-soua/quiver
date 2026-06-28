#!/usr/bin/env bash
# Build the per-release visual recap: a montage image + a gallery markdown.
# Cockpit screenshots are committed at the tag (raw URLs render in release notes);
# the montage is uploaded as a release asset and referenced by its download URL.
#
# Usage: visual-recap.sh <tag>
# Requires: ImageMagick `montage`, env GITHUB_REPOSITORY.
set -euo pipefail
tag="${1:?usage: visual-recap.sh <tag>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY not set}"
raw="https://raw.githubusercontent.com/$repo/$tag"
blob="https://github.com/$repo/blob/$tag"
dl="https://github.com/$repo/releases/download/$tag"

out="out/recap"
mkdir -p "$out"

# Hero montage from cockpit screenshots (PNGs render in release notes; SVGs do not).
shots=(dashboard constellation search help theme-slate demo-start)
files=()
for s in "${shots[@]}"; do
  [ -f "docs/assets/cockpit/$s.png" ] && files+=("docs/assets/cockpit/$s.png")
done
montage "${files[@]}" \
  -tile 2x3 -geometry 640x+10+10 -background black \
  -title "Quiver $tag — visual recap" \
  "$out/quiver-visual-recap.png"

# Gallery markdown — uploaded as an asset and appended to the release notes.
md="$out/visual-recap.md"
{
  echo "<!-- visual-recap -->"
  echo "### Visual recap"
  echo
  echo "[![Quiver $tag]($dl/quiver-visual-recap.png)]($blob/docs/diagrams.md)"
  echo
  echo "| | |"
  echo "|---|---|"
  echo "| ![Dashboard]($raw/docs/assets/cockpit/dashboard.png) | ![Constellation]($raw/docs/assets/cockpit/constellation.png) |"
  echo "| ![Search]($raw/docs/assets/cockpit/search.png) | ![Help]($raw/docs/assets/cockpit/help.png) |"
  echo
  echo "**More:** [subsystem diagrams]($blob/docs/diagrams.md) · [architecture]($blob/docs/architecture/overview.md) · [field guide PDF — \"Quiver, Explained\"]($raw/docs/quiver-explained.pdf)"
} >"$md"

echo "wrote $out/quiver-visual-recap.png and $md"
