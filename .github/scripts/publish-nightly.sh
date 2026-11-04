#!/usr/bin/env bash
set -euo pipefail

[[ "$GITHUB_REF" == 'refs/heads/feature/dev' ]]
[[ "$GITHUB_EVENT_NAME" == 'push' || "$GITHUB_EVENT_NAME" == 'workflow_dispatch' ]]
[[ "$GITHUB_SHA" =~ ^[a-f0-9]{40}$ ]]
[[ "$GITHUB_RUN_ID" =~ ^[0-9]+$ ]]

cd dist
shopt -s nullglob
assets=(*-nightly.*.zip *-nightly.*.tar.gz)
checksums=(*-nightly.*.sha256)
[[ ${#assets[@]} -eq 2 && ${#checksums[@]} -eq 2 ]]
for asset in "${assets[@]}"; do
  [[ "$asset" == *"-nightly.${GITHUB_RUN_ID}.${GITHUB_SHA:0:8}-"* ]]
  [[ -f "$asset.sha256" ]]
done
sha256sum --check "${checksums[@]}"

tag="nightly-${GITHUB_RUN_ID}-${GITHUB_SHA:0:8}"
title="Nightly ${GITHUB_RUN_ID} (${GITHUB_SHA:0:8})"
notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
printf 'Development build from feature/dev.

Commit: %s
Build: %s/%s/actions/runs/%s

Includes Windows x64 and Linux x64 binaries and SHA-256 checksums.
The package build-info.json identifies this nightly; executable versions retain the Cargo version.
' \
  "$GITHUB_SHA" "$GITHUB_SERVER_URL" "$GH_REPO" "$GITHUB_RUN_ID" > "$notes"

if gh release view "$tag" --json tagName >/dev/null 2>&1; then
  # Re-running this build may only replace assets on the same commit.
  is_draft=$(gh release view "$tag" --json isDraft --jq .isDraft)
  if [[ "$is_draft" == 'true' ]]; then
    # GitHub may not create the tag until a draft is published.
    published_sha=$(gh release view "$tag" --json targetCommitish --jq .targetCommitish)
  else
    published_sha=$(gh api "repos/$GH_REPO/commits/$tag" --jq .sha)
  fi
  [[ "$published_sha" == "$GITHUB_SHA" ]]
  gh release upload "$tag" "${assets[@]}" "${checksums[@]}" --clobber
else
  # Keep an incomplete upload hidden until all assets are attached.
  gh release create "$tag" "${assets[@]}" "${checksums[@]}" \
    --target "$GITHUB_SHA" --title "$title" --notes-file "$notes" \
    --draft --prerelease --latest=false
fi
gh release edit "$tag" --draft=false --prerelease --latest=false \
  --title "$title" --notes-file "$notes"
