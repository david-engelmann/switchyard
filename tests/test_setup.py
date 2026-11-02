import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MARKER = "<!-- repo annex below this line -->"


def read(name):
    return (ROOT / name).read_text(encoding="utf-8")


def test_seed_files_exist():
    assert (ROOT / "README.md").is_file()
    assert (ROOT / "LICENSE").is_file()
    assert (ROOT / ".gitignore").is_file()


def test_license_starts_with_mit_heading():
    text = read("LICENSE")
    assert text.startswith("MIT License")
    assert "Permission is hereby granted" in text


def test_agents_contains_marked_line():
    assert MARKER in read("AGENTS.md")


def test_agents_canonical_part_carries_program_discipline():
    above = read("AGENTS.md").split(MARKER)[0]
    assert "39 repo storefront perfection program" in above
    assert "The round discipline" in above
    assert "Contract first" in above
    assert "Honesty of record" in above


def test_agents_annex_below_marker_names_switchyard():
    below = read("AGENTS.md").split(MARKER)[1]
    assert "switchyard" in below
    assert "CONTRACT.md" in below
    assert "Mayfly makes the box" in below


def test_contract_names_setup_obligations():
    text = read("CONTRACT.md")
    for code in ("S1", "S2", "S3", "S4", "S5"):
        assert f"Obligation {code}" in text


def test_readme_status_states_zero_of_one_hundred():
    text = read("README.md")
    assert "## Status" in text
    assert "0 of 100" in text
    assert "e7e7d53dcd4d41fe5b46d0c51f1808872ff133fb" in text


def test_codepoint_scan_reports_zero_for_repo_tree():
    result = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "codepoint_scan.py"), str(ROOT)],
        capture_output=True,
        text=True,
    )
    assert "U+2014 em dash count 0" in result.stdout
    assert "U+2013 en dash count 0" in result.stdout
    assert result.returncode == 0
