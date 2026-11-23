//! The setup pins, translated from the Python setup suite the repo
//! was seeded with, brought up to the current fleet standard. Each
//! test names the same fact its Python original named, with the
//! strengthenings the fleet folded in after switchyard's Round 1
//! suite was written. Two facts changed with the Rust conversion
//! and the change is stated where it lands. The CI gate commands
//! are the Rust gate commands of amended obligation S2, and the
//! contract substance pin asserts the amended S2 phrases.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const MARKER: &str = "<!-- repo annex below this line -->";
const GATE_COMMAND: &str = "cargo test";
const SCAN_COMMAND: &str = "cargo run --bin codepoint_scan";
const FMT_COMMAND: &str = "cargo fmt --check";
const CLIPPY_COMMAND: &str = "cargo clippy --all-targets -- -D warnings";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(name: &str) -> String {
    std::fs::read_to_string(root().join(name)).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "switchyard-setup-test-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn seed_files_exist() {
    for name in ["README.md", "LICENSE", ".gitignore"] {
        assert!(root().join(name).is_file(), "{name}");
    }
}

#[test]
fn license_starts_with_mit_heading() {
    let text = read("LICENSE");
    assert!(text.starts_with("MIT License"));
    assert!(text.contains("Permission is hereby granted"));
}

#[test]
fn agents_has_marker_and_switchyard_annex_below_it() {
    let text = read("AGENTS.md");
    assert!(text.contains(MARKER));
    let annex = text.split(MARKER).nth(1).unwrap();
    assert!(annex.contains("switchyard"));
    assert!(annex.contains("CONTRACT.md"));
    assert!(annex.contains("Mayfly makes the box"));
}

#[test]
fn agents_canonical_part_carries_program_discipline() {
    let text = read("AGENTS.md");
    let canonical = text.split(MARKER).next().unwrap();
    assert!(canonical.contains("39 repo storefront perfection program"));
    assert!(canonical.contains("The round discipline"));
    assert!(canonical.contains("Contract first"));
    assert!(canonical.contains("Honesty of record"));
}

#[test]
fn contract_names_setup_obligations() {
    let text = read("CONTRACT.md");
    for code in ["S1", "S2", "S3", "S4", "S5"] {
        assert!(text.contains(&format!("Obligation {code}")), "{code}");
    }
}

fn status_section(text: &str) -> &str {
    let start = text.find("\n## Status\n").unwrap() + "\n## Status\n".len();
    let rest = &text[start..];
    match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

#[test]
fn readme_status_states_one_hundred_of_one_hundred() {
    let text = read("README.md");
    assert!(text.contains("## Status"));
    let status = status_section(&text);
    // The full sentence, never the bare count, because the
    // bare count is a substring of every two digit count that
    // ends in the same digit and a pin on it would pass over
    // a wrong statement of the closed rounds.
    assert!(status.contains("Closed perfection rounds are 100 of 100."));
    assert!(status.contains("e7e7d53dcd4d41fe5b46d0c51f1808872ff133fb"));
}

fn flush_step(step_lines: &mut Vec<String>, commands: &mut Vec<String>) {
    if step_lines.is_empty() {
        return;
    }
    let mut disabled = false;
    let mut runs = Vec::new();
    for stripped in step_lines.iter() {
        if let Some(value) = stripped.strip_prefix("if:") {
            let value = value
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_lowercase();
            if value == "false" {
                disabled = true;
            }
        }
        if let Some(pos) = stripped.find("run:") {
            runs.push(stripped[pos + "run:".len()..].trim().to_string());
        }
    }
    if !disabled {
        commands.extend(runs);
    }
    step_lines.clear();
}

/// A job head line of the workflow, a key standing at the two
/// space indent the jobs of ci.yml are keyed at, so the parser
/// can tell one job from the next in a file it reads as lines.
fn is_job_head(line: &str) -> bool {
    line.starts_with("  ")
        && !line.starts_with("   ")
        && line.trim_end().ends_with(':')
        && !line.trim().starts_with('-')
}

fn run_commands(ci_text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut step_lines: Vec<String> = Vec::new();
    let mut in_jobs = false;
    let mut in_steps = false;
    let mut in_test_job = false;
    let mut job_disabled = false;
    for line in ci_text.lines() {
        let stripped = line.trim();
        if stripped.starts_with('#') {
            continue;
        }
        if stripped == "jobs:" {
            in_jobs = true;
            continue;
        }
        if in_jobs && is_job_head(line) {
            // A new job ends the test job's steps. The run
            // commands of a later job are that job's own and
            // are never collected as the test job's, because
            // the gate this parser proves is the one the test
            // job runs.
            flush_step(&mut step_lines, &mut commands);
            in_test_job = stripped == "test:";
            in_steps = false;
            continue;
        }
        if !in_test_job {
            continue;
        }
        if !in_steps && stripped.starts_with("if:") {
            let value = stripped
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or_default()
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_lowercase();
            if value == "false" {
                job_disabled = true;
            }
            continue;
        }
        if stripped == "steps:" {
            in_steps = true;
            continue;
        }
        if in_steps && stripped.starts_with("- ") {
            flush_step(&mut step_lines, &mut commands);
            step_lines.push(stripped[2..].trim().to_string());
            continue;
        }
        if in_steps {
            step_lines.push(stripped.to_string());
        }
    }
    flush_step(&mut step_lines, &mut commands);
    if job_disabled {
        return Vec::new();
    }
    commands
}

#[test]
fn ci_runs_the_local_gate_commands() {
    let commands = run_commands(&read(".github/workflows/ci.yml"));
    assert!(commands.iter().any(|c| c == GATE_COMMAND));
    assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    assert!(commands.iter().any(|c| c == FMT_COMMAND));
    assert!(commands.iter().any(|c| c == CLIPPY_COMMAND));
}

#[test]
fn ci_gate_parser_does_not_credit_a_later_job_with_a_gate_command() {
    let ci_text = read(".github/workflows/ci.yml");
    // Move the scan step out of the test job and stand it at
    // the end of the file, where it belongs to the binding
    // job. The parser must still report the gate commands the
    // test job itself runs and must not report the scan,
    // because obligation S2 speaks of the gate the workflow's
    // test job runs, not of a command standing anywhere in
    // the file.
    let scan_step = concat!(
        "      - name: Run the codepoint scan\n",
        "        run: cargo run --bin codepoint_scan\n"
    );
    assert!(ci_text.contains(scan_step));
    let mut moved = ci_text.replace(scan_step, "");
    assert!(!moved.contains(SCAN_COMMAND));
    moved.push_str(scan_step);
    let commands = run_commands(&moved);
    assert!(commands.iter().any(|c| c == GATE_COMMAND));
    assert!(commands.iter().any(|c| c == FMT_COMMAND));
    assert!(commands.iter().any(|c| c == CLIPPY_COMMAND));
    assert!(!commands.iter().any(|c| c == SCAN_COMMAND));
}

#[test]
fn ci_carries_the_fleet_credential_hygiene() {
    let text = read(".github/workflows/ci.yml");
    // The workflow asks for no more than it uses. Its token
    // is limited to reading the repository, the checkout
    // leaves no credential behind for later steps, and the
    // toolchain action is pinned to a full commit SHA rather
    // than a moving branch name.
    assert!(text.contains("permissions:"));
    assert!(text.contains("contents: read"));
    assert!(text.contains("persist-credentials: false"));
    assert!(!text.contains("rust-toolchain@master"));
    // Every toolchain step stands judged, not only the first
    // one the file finds: the workflow carries two, one per
    // job, and a shortened pin on either fails this pin
    // (SY-A40).
    let toolchain_lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("dtolnay/rust-toolchain@"))
        .collect();
    assert!(
        !toolchain_lines.is_empty(),
        "the workflow carries no toolchain step"
    );
    for toolchain_line in toolchain_lines {
        let pin = toolchain_line
            .split("dtolnay/rust-toolchain@")
            .nth(1)
            .unwrap()
            .trim();
        assert_eq!(pin.len(), 40, "{}", toolchain_line);
        assert!(
            pin.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{}",
            toolchain_line
        );
    }
}

#[test]
fn ci_gate_parser_skips_a_step_disabled_with_if_false() {
    let ci_text = read(".github/workflows/ci.yml");
    let gate_step = "      - name: Run the local gate\n        run: cargo test";
    assert!(ci_text.contains(gate_step));
    for flag in ["false", "\"false\""] {
        let disabled = ci_text.replace(
            gate_step,
            &format!(
                "      - name: Run the local gate\n        if: {flag}\n        run: cargo test"
            ),
        );
        let commands = run_commands(&disabled);
        assert!(!commands.iter().any(|c| c == GATE_COMMAND));
        assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    }
}

#[test]
fn ci_gate_parser_rejects_a_job_disabled_with_if_false() {
    let ci_text = read(".github/workflows/ci.yml");
    let job_head = "  test:\n    runs-on: ubuntu-latest";
    assert!(ci_text.contains(job_head));
    for flag in ["false", "\"false\""] {
        let disabled = ci_text.replace(
            job_head,
            &format!("  test:\n    if: {flag}\n    runs-on: ubuntu-latest"),
        );
        let commands = run_commands(&disabled);
        assert!(!commands.iter().any(|c| c == GATE_COMMAND));
        assert!(!commands.iter().any(|c| c == SCAN_COMMAND));
        assert!(!commands.iter().any(|c| c == FMT_COMMAND));
        assert!(!commands.iter().any(|c| c == CLIPPY_COMMAND));
    }
    let commands = run_commands(&ci_text);
    assert!(commands.iter().any(|c| c == GATE_COMMAND));
    assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    assert!(commands.iter().any(|c| c == FMT_COMMAND));
    assert!(commands.iter().any(|c| c == CLIPPY_COMMAND));
}

struct ScanOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

fn scan_tree(root: &Path) -> ScanOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(root)
        .output()
        .unwrap();
    ScanOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn codepoint_scan_reports_zero_for_repo_tree() {
    let result = scan_tree(&root());
    assert_eq!(result.status, 0, "{}", result.stdout);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_with_no_argument_scans_the_current_directory() {
    // The scanner's module doc names the scan root defaulting
    // to the current directory among the port's deliberate
    // differences from the setup scanner, and the workflow scan
    // step names no root and relies on that default over the
    // checkout tree. Every other drive of the scanner in this
    // suite names its root, so the default stands pinned here.
    // A scanner defaulting to any fixed tree reports zero
    // counts over the staged tree at exit 0, and one refusing
    // with no argument exits 2, and this pin fails on both.
    let dir = temp_dir("no-argument");
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.lines().any(|line| line == "em dash U+2014 count 1"),
        "{stdout}"
    );
}

const CANONICAL_ABOVE_MARKER_SHA256: &str =
    "bb3e2fc1790b13729da3495c3f0902b968577f4e45ceb27e48e90b489f36e1a7";

#[test]
fn agents_canonical_part_matches_pinned_digest() {
    let agents = std::fs::read(root().join("AGENTS.md")).unwrap();
    let marker: &[u8] = b"<!-- repo annex below this line -->";
    let position = agents
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&agents[..position]);
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(digest, CANONICAL_ABOVE_MARKER_SHA256);
}

#[test]
fn readme_has_exactly_one_status_section() {
    let text = read("README.md");
    assert_eq!(text.matches("\n## Status\n").count(), 1);
}

#[test]
fn readme_carries_no_wrong_arc_label() {
    assert!(!read("README.md").contains("Fleet Operations"));
}

#[test]
fn codepoint_scan_counts_an_em_dash() {
    let dir = temp_dir("em-dash");
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
    assert!(result.stdout.contains("note.md"));
}

#[test]
fn codepoint_scan_counts_an_en_dash() {
    let dir = temp_dir("en-dash");
    std::fs::write(dir.join("note.md"), "a\u{2013}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("en dash U+2013 count 1"));
    assert!(result.stdout.contains("em dash U+2014 count 0"));
}

#[test]
fn codepoint_scan_scans_a_root_under_an_excluded_ancestor() {
    let dir = temp_dir("excluded-ancestor").join("venv").join("tree");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_skips_excluded_dirs_below_the_root() {
    let dir = temp_dir("excluded-dirs");
    let cache = dir.join("build");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("note.md"), "a\u{2014}b").unwrap();
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn contract_obligations_carry_their_substance() {
    let text = read("CONTRACT.md");
    for phrase in [
        "present from the first commit",
        "lands through pull requests",
        "no paid service",
        "cargo test plus the codepoint scan run through cargo",
        "0 of 100 until a full cycle",
        "no real trace and no real transcript",
        "SHA-256 digest of the part above the marked line",
    ] {
        assert!(text.contains(phrase), "{phrase}");
    }
}

#[test]
fn codepoint_scan_fails_loudly_on_undecodable_input() {
    let dir = temp_dir("undecodable");
    std::fs::write(dir.join("note.md"), b"\xff\xfe not utf-8").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 2);
    assert!(result.stderr.contains("note.md"));
}

#[test]
fn codepoint_scan_refuses_a_root_that_does_not_stand() {
    // The scanner's module doc names a scan root that does not
    // stand as a scan error at exit 2, one of its four deliberate
    // differences from the setup scanner, which exited 0 with
    // zero counts there. The pin drives the built scanner at a
    // root that is never created, so it fails on any form that
    // falls back to a default root and reports zero counts, and
    // on any form that panics on the missing root at exit 101.
    let dir = temp_dir("missing-root").join("no-such-root");
    let result = scan_tree(&dir);
    assert_eq!(result.status, 2);
    let root = dir.display().to_string();
    assert!(
        result
            .stderr
            .lines()
            .any(|line| line.contains("scan error") && line.contains(&root)),
        "{}",
        result.stderr
    );
}

#[test]
fn codepoint_scan_counts_a_dash_in_a_file_with_no_suffix() {
    let dir = temp_dir("no-suffix");
    std::fs::write(dir.join("LICENSE"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_counts_a_dash_in_an_uppercase_suffix_file() {
    let dir = temp_dir("upper-suffix");
    std::fs::write(dir.join("UPPER.MD"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_skips_a_file_with_an_uppercase_binary_suffix() {
    let dir = temp_dir("upper-binary");
    std::fs::write(dir.join("FAKE.PNG"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[cfg(target_os = "linux")]
#[test]
fn codepoint_scan_reports_a_failed_output_write_at_exit_2() {
    use std::process::Stdio;
    // The scanner writes its whole output through one checked
    // writer, so a stdout that refuses the write lands the scan
    // at its stated error exit instead of panicking inside a
    // print (SY-A42). The drive stages /dev/full as the output,
    // which answers every write with no space left; before the
    // repair the count lines went through println! and this
    // drive died at exit 101.
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(root())
        .stdout(Stdio::from(full))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot write the scan output"), "{stderr}");
}
