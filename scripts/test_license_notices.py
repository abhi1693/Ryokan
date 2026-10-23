from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_license_notices import validate_notices


def notices(*pairs):
    return '<ul class="license-used-by">' + ''.join(
        f'<li><a href="https://example.invalid">{name} {version}</a></li>'
        for name, version in pairs) + '</ul>'


LOCK = '''version = 4
[[package]]
name = "ctutils"
version = "0.4.4"
[[package]]
name = "same-name"
version = "1.0.0"
[[package]]
name = "same-name"
version = "2.0.0"
'''


class LicenseNoticeTests(unittest.TestCase):
    def test_exact_pairs_and_multiple_locked_versions(self):
        result = validate_notices(notices(('ctutils', '0.4.4'),
            ('same-name', '1.0.0'), ('same-name', '2.0.0')), LOCK)
        self.assertEqual(result['unique_pairs'], 3)

    def test_stale_wrong_and_unknown_pairs_fail(self):
        for pair in [('ctutils', '0.4.3'), ('same-name', '3.0.0'), ('unknown', '1.0.0')]:
            with self.subTest(pair=pair), self.assertRaises(ValueError):
                validate_notices(notices(pair), LOCK)

    def test_empty_malformed_and_incomplete_parse_fail(self):
        for html in ['', '<p>ctutils 0.4.4</p>', notices(),
                     '<ul class="license-used-by"><li>ctutils 0.4.4</li></ul>',
                     '<ul class="license-used-by"><li><a>ctutils 0.4.4</a></li>',
                     notices(('ctutils', 'garbage'))]:
            with self.subTest(html=html), self.assertRaises(ValueError):
                validate_notices(html, LOCK)
        with self.assertRaises(ValueError):
            validate_notices(notices(('ctutils', '0.4.4')), 'version = 4')

    def test_repeated_notice_pairs_are_checked_without_losing_versions(self):
        pair = ('ctutils', '0.4.4')
        result = validate_notices(notices(pair) + notices(pair), LOCK)
        self.assertEqual((result['unique_pairs'], result['entries']), (1, 2))
        self.assertEqual(validate_notices(notices(pair, pair), LOCK)['entries'], 2)
        with self.assertRaises(ValueError):
            validate_notices(notices(('ctutils', '0.4.3'), ('ctutils', '0.4.3')), LOCK)

    def test_import_performs_no_repository_check(self):
        script = Path(__file__).with_name('check_license_notices.py')
        with tempfile.TemporaryDirectory() as empty:
            code = ('import importlib.util; '
                    f's=importlib.util.spec_from_file_location("notice_guard", {str(script)!r}); '
                    'm=importlib.util.module_from_spec(s); s.loader.exec_module(m)')
            result = subprocess.run([sys.executable, '-I', '-B', '-c', code],
                                    cwd=empty, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, '')

    def test_committed_notices_match_current_lock(self):
        root = Path(__file__).resolve().parents[1]
        result = validate_notices((root/'licenses/THIRD_PARTY_LICENSES.html').read_text(),
                                  (root/'Cargo.lock').read_text())
        self.assertGreater(result['unique_pairs'], 0)


if __name__ == '__main__':
    unittest.main()
