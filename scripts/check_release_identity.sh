#!/usr/bin/env bash
set -euo pipefail

if [[ "${GITHUB_REF_TYPE:-}" != tag ]]; then
  echo 'Container publication requires the immutable matching release tag.' >&2
  exit 1
fi

version=$(python3 -c 'import sys; from pathlib import Path; sys.path.insert(0, "scripts"); from check_image_version import source_version; print(source_version(Path(".")))')
[[ "${GITHUB_REF_NAME:-}" == "v${version}" ]]
[[ "${GITHUB_REF:-}" == "refs/tags/v${version}" ]]
[[ -z "${REQUESTED_VERSION:-}" || "${REQUESTED_VERSION}" == "$version" ]]
[[ -z "${RELEASE_TAG_NAME:-}" || "${RELEASE_TAG_NAME}" == "${GITHUB_REF_NAME}" ]]
tag_commit=$(git rev-parse --verify "refs/tags/${GITHUB_REF_NAME}^{commit}")
[[ "$tag_commit" == "${GITHUB_SHA:-}" ]]
[[ "$(git rev-parse HEAD)" == "${GITHUB_SHA:-}" ]]
