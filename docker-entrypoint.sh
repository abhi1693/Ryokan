#!/bin/sh
# Ryokan Docker entrypoint.
#
# Creates (or updates) a `ryokan` user whose UID/GID match the PUID/PGID
# environment variables, fixes ownership on the persistent /data volume,
# then drops privileges via gosu before execing the Ryokan binary.
#
# This matches the linuxserver.io PUID/PGID convention used by the
# Sonarr/Radarr/Jellyfin ecosystem so files written by Ryokan's
# post-processor share ownership with the rest of a typical *arr stack.

set -e

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
# Keep the encryption key on the persistent data volume while still allowing
# operators to override its location. Defining this here avoids treating the
# path itself as secret image metadata during Dockerfile validation.
RYOKAN_KEY_FILE_PATH="${RYOKAN_KEY_FILE_PATH:-/data/.ryokan-key}"
export RYOKAN_KEY_FILE_PATH

# --- Group ---
if ! getent group ryokan >/dev/null 2>&1; then
    groupadd -o -g "$PGID" ryokan
else
    current_gid=$(getent group ryokan | cut -d: -f3)
    if [ "$current_gid" != "$PGID" ]; then
        groupmod -o -g "$PGID" ryokan
    fi
fi

# --- User ---
if ! id -u ryokan >/dev/null 2>&1; then
    useradd -o -u "$PUID" -g "$PGID" -d /data -s /usr/sbin/nologin ryokan
else
    current_uid=$(id -u ryokan)
    if [ "$current_uid" != "$PUID" ]; then
        usermod -o -u "$PUID" ryokan
    fi
fi

# --- Data volume ownership ---
# Only touch files that aren't already correctly owned. On a warm start
# this is a no-op scan; on a PUID change it quietly re-chowns everything.
# The user-mounted /downloads and /media/* paths are intentionally left
# alone — those belong to the host.
find /data \! -user ryokan -exec chown ryokan:ryokan {} + 2>/dev/null || true

exec gosu ryokan "$@"
