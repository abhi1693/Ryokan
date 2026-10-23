"""Use Cargo's application version for images and reject source/tag mismatches."""

import os
import re
import tomllib
from pathlib import Path


def source_version(root):
    package = tomllib.loads((root / "Cargo.toml").read_text())["package"]
    version = package["version"]
    locked = [
        item["version"]
        for item in tomllib.loads((root / "Cargo.lock").read_text())["package"]
        if item["name"] == package["name"]
    ]
    if locked != [version]:
        raise ValueError("Application Cargo source and lock versions differ")
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("Expected a semantic application version")
    return version


def check_version(version, environment):
    supplied = environment.get("RELEASE_VERSION", "")
    if supplied and supplied.removeprefix("v") != version:
        raise ValueError("Requested image version differs from application source")
    if (
        environment.get("GITHUB_REF_TYPE") == "tag"
        and environment.get("GITHUB_REF_NAME", "").removeprefix("v") != version
    ):
        raise ValueError("Release tag differs from application source")
    return version


def main():
    version = check_version(
        source_version(Path(__file__).resolve().parents[1]), os.environ
    )
    if output := os.environ.get("GITHUB_OUTPUT"):
        with Path(output).open("a") as stream:
            stream.write(f"version={version}\n")
    print(version)


if __name__ == "__main__":
    main()
