"""Exercise the shipped entrypoint and HTTP application using isolated local data."""

import hashlib
import io
import json
import re
import secrets
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid
import wave
from pathlib import Path

DISTRUSTED = {
    "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b",
    "d7a7a0fb5d7e2731d771e9484ebcdef71d5f0c3e0a2948782bc83ee0ea699ef4",
}


def docker(*arguments, data=None, check=True):
    result = subprocess.run(
        ["docker", *arguments],
        input=data,
        capture_output=True,
        timeout=90,
        check=False,
    )
    if check and result.returncode:
        # Never print HTTP cookies, synthetic credentials or application data.
        raise RuntimeError(
            f"Docker {arguments[0]} failed with exit {result.returncode}"
        )
    return result


def http(container, path, *, cookie=None, form=None):
    arguments = [
        "exec",
        container,
        "curl",
        "--noproxy",
        "*",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "2",
        "--max-time",
        "8",
        "--include",
    ]
    if cookie:
        arguments += ["--header", "Cookie: " + cookie]
    if form is not None:
        arguments += [
            "--header",
            "Origin: http://127.0.0.1:8978",
            "--data",
            urllib.parse.urlencode(form),
        ]
    raw = docker(*arguments, "http://127.0.0.1:8978" + path).stdout
    headers, body = raw.split(b"\r\n\r\n", 1)
    lines = headers.decode("latin1").split("\r\n")
    fields = {}
    for line in lines[1:]:
        if ": " in line:
            key, value = line.split(": ", 1)
            fields[key.lower()] = value
    return int(lines[0].split()[1]), fields, body


def ready(container):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if (
            docker("inspect", "--format", "{{.State.Running}}", container).stdout
            != b"true\n"
        ):
            raise RuntimeError("The application exited before HTTP readiness")
        try:
            status, _, _ = http(container, "/login")
            if status in {200, 303}:
                return
        except RuntimeError:
            pass
        time.sleep(0.25)
    raise RuntimeError("The real /login endpoint did not become ready within 60s")


def runtime_contract(container, version, platform):
    status = docker("exec", container, "cat", "/proc/1/status").stdout.decode()
    for field in ["Uid", "Gid"]:
        match = re.search(rf"^{field}:\s+(.+)$", status, re.MULTILINE)
        assert match and set(match[1].split()) == {"568"}, (
            "PID 1 did not drop privileges"
        )
    ownership = docker("exec", container, "stat", "-c", "%u:%g", "/data").stdout.strip()
    assert ownership == b"568:568", "Persistent data ownership does not match PUID/PGID"
    linkage = docker("exec", container, "ldd", "/app/ryokan").stdout
    assert b"not found" not in linkage, (
        "The application has unresolved native libraries"
    )
    assert b"libstdc++.so.6" in linkage, "The native title parser ABI was not checked"
    gosu = docker("exec", container, "gosu", "--version").stdout.decode().strip()
    assert gosu == f"1.19 (go1.27.3 on {platform}; gc)", (
        "gosu compiler/version differs from policy"
    )
    assert (
        docker("exec", container, "sh", "-c", "command -v gosu").stdout.strip()
        == b"/usr/local/bin/gosu"
    )
    assert (
        docker("exec", container, "dpkg-query", "-W", "gosu", check=False).returncode
        != 0
    )
    docker(
        "exec",
        container,
        "sh",
        "-c",
        "test -s /var/lib/dpkg/status && test ! -e /usr/bin/pebble",
    )
    bundle = docker(
        "exec", container, "cat", "/etc/ssl/certs/ca-certificates.crt"
    ).stdout.decode()
    roots = {
        hashlib.sha256(ssl.PEM_cert_to_DER_cert(pem)).hexdigest()
        for pem in re.findall(
            r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----",
            bundle,
            re.DOTALL,
        )
    }
    assert len(roots) >= 100 and not roots & DISTRUSTED, (
        "System root trust policy failed"
    )
    healthcheck = json.loads(
        docker(
            "inspect", "--format", "{{json .Config.Healthcheck.Test}}", container
        ).stdout
    )
    assert healthcheck[0] == "CMD-SHELL" and "/login" in healthcheck[1]
    docker("exec", container, "sh", "-c", healthcheck[1])
    return {
        "source_version": version,
        "pid1_uid_gid": "568:568",
        "gosu": gosu,
        "trusted_roots": len(roots),
    }


def main(image, platform, version):
    name = "ryokan-smoke-" + uuid.uuid4().hex[:12]
    volume = name + "-data"
    docker("volume", "create", volume)
    try:
        with tempfile.TemporaryDirectory(prefix="ryokan-smoke-") as directory:
            root = Path(directory)
            for path in [root / "downloads", root / "media"]:
                path.mkdir(mode=0o755)
                (path / "untouched.txt").write_text("Local runtime fixture only\n")
            before = {
                str(p): (p.stat().st_uid, p.stat().st_gid, p.read_bytes())
                for p in root.glob("*/*")
            }
            docker(
                "run",
                "--detach",
                "--name",
                name,
                "--platform",
                platform,
                "--network",
                "none",
                "--security-opt",
                "no-new-privileges",
                "--memory",
                "768m",
                "--pids-limit",
                "128",
                "--tmpfs",
                "/tmp:rw,nosuid,nodev,size=32m,mode=1777",
                "--mount",
                f"type=volume,source={volume},target=/data",
                "--mount",
                f"type=bind,source={root / 'downloads'},target=/downloads,readonly",
                "--mount",
                f"type=bind,source={root / 'media'},target=/media/anime,readonly",
                "--env",
                "PUID=568",
                "--env",
                "PGID=568",
                image,
            )
            ready(name)
            proof = runtime_contract(name, version, platform)
            key_before = docker("exec", name, "cat", "/data/.ryokan-key").stdout
            assert len(key_before) == 32
            assert (
                docker(
                    "exec", name, "stat", "-c", "%u:%g:%a", "/data/.ryokan-key"
                ).stdout.strip()
                == b"568:568:600"
            )
            status, headers, _ = http(name, "/login")
            assert status == 303 and headers.get("location") == "/setup"
            assert http(name, "/setup")[0] == 200
            fixture_password = secrets.token_urlsafe(32)
            status, headers, _ = http(
                name,
                "/setup",
                form={
                    "username": "runtime-fixture",
                    "password": fixture_password,
                    "confirm": fixture_password,
                },
            )
            assert status == 303 and headers.get("location") == "/"
            cookie = headers["set-cookie"].split(";", 1)[0]
            assert cookie.startswith("session=") and len(cookie) > 20
            status, _, body = http(name, "/settings?tab=general", cookie=cookie)
            assert status == 200, f"Authenticated settings returned HTTP {status}"
            assert f"Ryokan v{version}".encode() in body, (
                "Rendered application versions: "
                + str(re.findall(rb"Ryokan v([0-9.]+)", body))
            )
            status, _, body = http(name, "/api/health", cookie=cookie)
            assert status == 200
            assert json.loads(body) == {
                "download_client": {"ok": False, "message": "Not configured"},
                "jellyfin": {"ok": False, "message": "Not configured"},
            }
            assert http(name, "/settings")[0] in {302, 303}
            assert http(name, "/login")[0] == 200
            for asset in ["/static/css/base.css", "/static/vendor/htmx-2.0.11.min.js"]:
                status, _, body = http(name, asset, cookie=cookie)
                assert status == 200 and len(body) > 100, (
                    "Packaged static asset missing"
                )
            audio = io.BytesIO()
            assert docker("exec", name, "ffprobe", "-version").stdout.startswith(
                b"ffprobe version 9.0.2"
            )
            with wave.open(audio, "wb") as wav:
                wav.setparams((1, 2, 8000, 0, "NONE", "not compressed"))
                wav.writeframes(b"\x00\x00" * 8000)
            probe = json.loads(
                docker(
                    "exec",
                    "-i",
                    name,
                    "ffprobe",
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-of",
                    "json",
                    "pipe:0",
                    data=audio.getvalue(),
                ).stdout
            )
            assert probe["streams"][0]["codec_name"] == "pcm_s16le"
            assert probe["format"]["format_name"] == "wav"
            assert probe["streams"][0]["sample_rate"] == "8000"
            docker("restart", "--time", "10", name)
            ready(name)
            assert http(name, "/login")[0] == 200, (
                "First-run account did not survive restart"
            )
            status, _, body = http(name, "/settings?tab=general", cookie=cookie)
            assert status == 200 and f"Ryokan v{version}".encode() in body
            runtime_contract(name, version, platform)
            assert docker("exec", name, "cat", "/data/.ryokan-key").stdout == key_before
            assert before == {
                str(p): (p.stat().st_uid, p.stat().st_gid, p.read_bytes())
                for p in root.glob("*/*")
            }
            docker(
                "run",
                "--rm",
                "--platform",
                platform,
                "--network",
                "none",
                "--security-opt",
                "no-new-privileges",
                "--memory",
                "64m",
                "--pids-limit",
                "32",
                "--tmpfs",
                "/data:rw,nosuid,nodev,size=8m",
                "--env",
                "PUID=1000",
                "--env",
                "PGID=1001",
                image,
                "sh",
                "-c",
                'test "$(id -u)" = 1000 && test "$(id -g)" = 1001 && test "$(stat -c %u:%g /data)" = 1000:1001',
            )
            proof.update(
                {
                    "platform": platform,
                    "http_setup_and_auth": "PASS",
                    "warm_restart": "PASS",
                    "encryption_key_persisted": True,
                    "ffprobe_decode": "PASS",
                    "alternate_puid_pgid": "1000:1001",
                    "external_network": False,
                    "media_unchanged": True,
                }
            )
            print(json.dumps(proof, sort_keys=True))
    finally:
        docker("rm", "--force", name, check=False)
        docker("volume", "rm", volume)


if __name__ == "__main__":
    main(*sys.argv[1:])
