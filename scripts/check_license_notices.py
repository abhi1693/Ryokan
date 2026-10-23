"""Check listed cargo-about crate/version pairs against Cargo.lock, not coverage."""

from html.parser import HTMLParser
from pathlib import Path
import re
import tomllib

PAIR = re.compile(r"([A-Za-z0-9_-]+) ([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)\Z")


class NoticeParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.section = None
        self.entry = None
        self.link = False
        self.links = 0
        self.pairs = set()
        self.entries = 0
        self.sections = 0

    def handle_starttag(self, tag, attrs):
        if tag == "ul" and "license-used-by" in dict(attrs).get("class", "").split():
            if self.section is not None:
                raise ValueError("Nested notice lists are unsupported")
            self.section = set()
            self.sections += 1
        elif self.section is not None:
            if tag == "li" and self.entry is None:
                self.entry = []
                self.links = 0
            elif tag == "a" and self.entry is not None and not self.link:
                self.link = True
                self.links += 1
            else:
                raise ValueError("Unexpected markup in a notice list")

    def handle_data(self, data):
        if self.section is not None:
            if self.entry is not None:
                self.entry.append(data)
            elif data.strip():
                raise ValueError("Unexpected text outside a notice entry")

    def handle_endtag(self, tag):
        if self.section is None:
            return
        if tag == "a" and self.link:
            self.link = False
        elif tag == "li" and self.entry is not None and not self.link:
            text = " ".join("".join(self.entry).split())
            match = PAIR.fullmatch(text)
            if self.links != 1 or match is None:
                raise ValueError("Expected one linked crate/version per notice entry")
            pair = match.groups()
            self.section.add(pair)
            self.pairs.add(pair)
            self.entries += 1
            self.entry = None
        elif tag == "ul" and self.entry is None and self.section:
            self.section = None
        else:
            raise ValueError("Incomplete or empty notice list")


def validate_notices(html, lock_text):
    packages = tomllib.loads(lock_text).get("package", [])
    if not packages or any(
        not isinstance(item.get("name"), str) or not item["name"]
        or not isinstance(item.get("version"), str) or not item["version"]
        for item in packages
    ):
        raise ValueError("Cargo.lock must contain named, versioned packages")
    locked = {(item["name"], item["version"]) for item in packages}
    parser = NoticeParser()
    parser.feed(html)
    parser.close()
    if not parser.pairs or parser.section is not None or parser.entry is not None:
        raise ValueError("No complete nonempty license-used-by lists parsed")
    stale = parser.pairs - locked
    if stale:
        raise ValueError("Notices absent from Cargo.lock: " + ", ".join(
            f"{name} {version}" for name, version in sorted(stale)))
    return {"unique_pairs": len(parser.pairs), "entries": parser.entries,
            "license_sections": parser.sections, "locked_pairs": len(locked)}


def main():
    root = Path(__file__).resolve().parents[1]
    result = validate_notices((root / "licenses/THIRD_PARTY_LICENSES.html").read_text(),
                              (root / "Cargo.lock").read_text())
    print(f"License notice membership passed: {result['unique_pairs']} unique pairs "
          f"({result['entries']} entries); complete dependency coverage is not asserted.")


if __name__ == "__main__":
    main()
