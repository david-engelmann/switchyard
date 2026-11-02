"""Count em dash and en dash codepoints across the repository tree.

The scan reads each matching file as text and counts the U+2014 and
U+2013 codepoints directly in Python. It exits with status 1 when
either count is nonzero and with status 0 when both counts are zero.
"""

import sys
from pathlib import Path

EXTENSIONS = {".md", ".txt", ".py", ".yml", ".toml"}
EXCLUDED_DIRS = {".git", "__pycache__", ".pytest_cache", ".venv", "venv"}

EM_DASH = "\u2014"
EN_DASH = "\u2013"


def iter_files(root):
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        if any(part in EXCLUDED_DIRS for part in path.parts):
            continue
        if path.suffix in EXTENSIONS:
            yield path


def main():
    if len(sys.argv) > 1:
        root = Path(sys.argv[1]).resolve()
    else:
        root = Path(__file__).resolve().parent.parent
    em_count = 0
    en_count = 0
    for path in iter_files(root):
        text = path.read_text(encoding="utf-8", errors="ignore")
        em_count += text.count(EM_DASH)
        en_count += text.count(EN_DASH)
    print(f"U+2014 em dash count {em_count}")
    print(f"U+2013 en dash count {en_count}")
    if em_count or en_count:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
