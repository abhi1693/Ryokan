#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
image=${1:?Usage: smoke-runtime.sh IMAGE PLATFORM [APP_VERSION]}
platform=${2:?Usage: smoke-runtime.sh IMAGE PLATFORM [APP_VERSION]}
source_version=$(python3 scripts/check_image_version.py)
test "${3:-$source_version}" = "$source_version"
case "$(uname -m):$platform" in
  x86_64:linux/amd64|aarch64:linux/arm64) ;;
  *) echo 'Runtime smoke requires a matching native runner' >&2; exit 1 ;;
esac
test "$(docker image inspect "$image" --format '{{.Os}}/{{.Architecture}}')" = "$platform"
test "$(docker image inspect "$image" --format '{{index .Config.Labels "org.opencontainers.image.version"}}')" = "$source_version"
test "$(docker image inspect "$image" --format '{{json .Config.Entrypoint}}')" = '["/usr/local/bin/docker-entrypoint.sh"]'
case "$(docker image inspect "$image" --format '{{.Config.User}}')" in
  ''|0|root|0:0|root:root) ;;
  *) echo 'The entrypoint must start as root before dropping privileges' >&2; exit 1 ;;
esac
python3 scripts/runtime_smoke.py "$image" "$platform" "$source_version"
