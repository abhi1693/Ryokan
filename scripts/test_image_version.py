"""Exercise actual source/lock/tag guards without building or publishing an image."""

import tempfile
import unittest
from pathlib import Path

from check_image_version import check_version, source_version


class ImageVersionTests(unittest.TestCase):
    def test_checked_source_version_matches_branch_and_tag(self):
        version = source_version(Path(__file__).resolve().parents[1])
        self.assertEqual(version, check_version(version, {}))
        self.assertEqual(
            version,
            check_version(
                version,
                {
                    "GITHUB_REF_TYPE": "tag",
                    "GITHUB_REF_NAME": f"v{version}",
                    "RELEASE_VERSION": version,
                },
            ),
        )

    def test_manual_or_tag_mismatch_is_rejected(self):
        for environment in [
            {"RELEASE_VERSION": "9.9.9"},
            {"GITHUB_REF_TYPE": "tag", "GITHUB_REF_NAME": "v9.9.9"},
        ]:
            with self.subTest(environment=environment), self.assertRaises(ValueError):
                check_version("1.9.13", environment)

    def test_lock_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[package]\nname="ryokan"\nversion="1.9.13"\n'
            )
            (root / "Cargo.lock").write_text(
                '[[package]]\nname="ryokan"\nversion="1.9.12"\n'
            )
            with self.assertRaises(ValueError):
                source_version(root)


if __name__ == "__main__":
    unittest.main()
