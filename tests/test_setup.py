import hashlib
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


def test_readme_status_states_one_of_one_hundred():
    text = read("README.md")
    assert "## Status" in text
    assert "1 of 100" in text
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


CANONICAL_ABOVE_MARKER_SHA256 = "bb3e2fc1790b13729da3495c3f0902b968577f4e45ceb27e48e90b489f36e1a7"


def test_agents_canonical_part_matches_pinned_digest():
    agents = (ROOT / "AGENTS.md").read_bytes()
    marker = b"<!-- repo annex below this line -->"
    above = agents[: agents.find(marker)]
    assert hashlib.sha256(above).hexdigest() == CANONICAL_ABOVE_MARKER_SHA256


def test_readme_has_exactly_one_status_section():
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert readme.count("\n## Status\n") == 1



def _run_commands(ci_text):
    commands = []
    for line in ci_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if "run:" in stripped:
            commands.append(stripped.split("run:", 1)[1].strip())
    return commands


def test_ci_runs_the_local_gate_commands():
    commands = _run_commands(read(".github/workflows/ci.yml"))
    assert "python -m pytest -q" in commands
    assert "python scripts/codepoint_scan.py" in commands


def _scan_tree(root):
    return subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "codepoint_scan.py"), str(root)],
        capture_output=True,
        text=True,
    )


def test_codepoint_scan_counts_an_em_dash(tmp_path):
    (tmp_path / "note.md").write_text("a\u2014b", encoding="utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 1
    assert "U+2014 em dash count 1" in result.stdout
    assert "note.md" in result.stdout


def test_codepoint_scan_counts_an_en_dash(tmp_path):
    (tmp_path / "note.md").write_text("a\u2013b", encoding="utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 1
    assert "U+2013 en dash count 1" in result.stdout


def test_codepoint_scan_scans_a_root_under_an_excluded_ancestor(tmp_path):
    root = tmp_path / "venv" / "tree"
    root.mkdir(parents=True)
    (root / "note.md").write_text("a\u2014b", encoding="utf-8")
    result = _scan_tree(root)
    assert result.returncode == 1
    assert "U+2014 em dash count 1" in result.stdout


def test_codepoint_scan_skips_excluded_dirs_below_the_root(tmp_path):
    cache = tmp_path / "build"
    cache.mkdir()
    (cache / "note.md").write_text("a\u2014b", encoding="utf-8")
    (tmp_path / "clean.md").write_text("clean", encoding="utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 0
    assert "U+2014 em dash count 0" in result.stdout


def test_contract_obligations_carry_their_substance():
    text = read("CONTRACT.md")
    for phrase in (
        "present from the first commit",
        "no network access and no paid service",
        "closed perfection rounds are 0 of 100",
        "no real trace and no real transcript",
        "SHA-256 digest of the part above the marked line",
    ):
        assert phrase in text, phrase


def test_codepoint_scan_fails_loudly_on_undecodable_input(tmp_path):
    (tmp_path / "note.md").write_bytes(b"\xff\xfe not utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 2
    assert "note.md" in result.stderr


def test_codepoint_scan_counts_a_dash_in_a_file_with_no_suffix(tmp_path):
    (tmp_path / "LICENSE").write_text("a\u2014b", encoding="utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 1
    assert "U+2014 em dash count 1" in result.stdout


def test_codepoint_scan_counts_a_dash_in_an_uppercase_suffix_file(tmp_path):
    (tmp_path / "UPPER.MD").write_text("a\u2014b", encoding="utf-8")
    result = _scan_tree(tmp_path)
    assert result.returncode == 1
    assert "U+2014 em dash count 1" in result.stdout
