//! Tests for the switchyard slice 1 obligations SY-1 to SY-10,
//! translated pin for pin from the Python suite of Round 1.
//!
//! Every fixture is synthetic. Stacks are built in temporary
//! directories, the git repositories inside them are created by the
//! init command under test, and the concurrency tests race real
//! operating system processes of the switchyard binary against one
//! registry behind a shared start signal.
//!
//! Three Python tests patched private functions to stage failures
//! (the incomplete rollback report, the git environment scrub and
//! timeout). Those pins live in the crate's unit tests against the
//! same private seams, the way the Python tests reached into
//! privates too. The failed registry save pin is staged against a
//! real file system shape in this file. The module entry point pin
//! drives the switchyard binary, which is what the crate ships in
//! place of the module. Registry fixtures written by the Python
//! implementation are checked in under tests/fixtures and exercised
//! at the end of this file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use switchyard::{ENV_NAME, LOCK_NAME, REGISTRY_NAME, WORKTREES_NAME};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let number = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "switchyard-test-{}-{number}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_cli(args: &[&str]) -> (i32, String, String) {
    run_cli_full(args, &[], None)
}

fn run_cli_full(args: &[&str], env: &[(&str, &str)], cwd: Option<&Path>) -> (i32, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_switchyard"));
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn init_stack_dir(base: &Path, pool: &[(&str, i64)]) -> PathBuf {
    let stack = base.join("stack");
    let mut args: Vec<String> = vec![
        "init".to_string(),
        "--stack".to_string(),
        stack.to_string_lossy().into_owned(),
    ];
    for (key, value) in pool {
        args.push(format!("--{}", key.replace('_', "-")));
        args.push(value.to_string());
    }
    let refs: Vec<&str> = args.iter().map(|arg| arg.as_str()).collect();
    let (code, _, err) = run_cli(&refs);
    assert_eq!(code, 0, "{err}");
    stack
}

fn stack_fixture() -> PathBuf {
    let base = temp_dir("stack");
    init_stack_dir(&base, &[])
}

fn read_registry(stack: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn git_in(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git(repo: &Path, args: &[&str]) -> String {
    git_in(repo, args)
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.symlink_metadata().unwrap().is_dir() {
                out.insert(rel, None);
                walk(&path, root, out);
            } else {
                out.insert(rel, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn claim_ok(stack: &Path, agent: &str) {
    let (code, _, err) = run_cli(&["claim", agent, "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
}

// SY-1, complete allocation

#[test]
fn init_records_the_pool_and_creates_the_repository() {
    let stack = stack_fixture();
    let data = read_registry(&stack);
    assert_eq!(data["version"], 1);
    assert_eq!(
        data["port_pool"],
        serde_json::json!({"start": 20000, "range_size": 10, "range_count": 16})
    );
    assert_eq!(data["claims"], serde_json::json!({}));
    let head = git(&stack.join("repo"), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(head.trim(), "main");
}

#[test]
fn claim_allocates_everything_and_the_record_matches_the_disk() {
    let stack = stack_fixture();
    let (code, out, err) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let data = read_registry(&stack);
    let claims = data["claims"].as_object().unwrap();
    assert_eq!(claims.keys().collect::<Vec<_>>(), vec!["alpha"]);
    let record = &claims["alpha"];
    assert_eq!(record["branch"], "switchyard/alpha");
    assert_eq!(record["database"], "switchyard_alpha");
    assert_eq!(record["port_start"], 20000);
    assert_eq!(record["port_end"], 20009);
    let pool = &data["port_pool"];
    let pool_end = pool["start"].as_i64().unwrap()
        + pool["range_size"].as_i64().unwrap() * pool["range_count"].as_i64().unwrap()
        - 1;
    assert!(pool["start"].as_i64().unwrap() <= record["port_start"].as_i64().unwrap());
    assert!(record["port_end"].as_i64().unwrap() <= pool_end);
    let worktree = stack.join(record["worktree"].as_str().unwrap());
    assert!(worktree.is_dir());
    let head = git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(head.trim(), "switchyard/alpha");
    let branches = git(
        &stack.join("repo"),
        &["branch", "--list", "switchyard/alpha"],
    );
    assert!(branches.contains("switchyard/alpha"));
    let env_file = stack.join(record["env_file"].as_str().unwrap());
    assert!(env_file.is_file());
    assert_eq!(env_file.parent().unwrap(), worktree);
    assert!(out.contains("switchyard/alpha"));
    assert!(out.contains("20000-20009"));
}

#[test]
fn env_file_contents_match_the_registry_record() {
    let stack = stack_fixture();
    claim_ok(&stack, "beta-one");
    let data = read_registry(&stack);
    let record = &data["claims"]["beta-one"];
    let text = std::fs::read_to_string(stack.join(record["env_file"].as_str().unwrap())).unwrap();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        if line.contains('=') && !line.starts_with('#') {
            let (key, value) = line.split_once('=').unwrap();
            env.insert(key.to_string(), value.to_string());
        }
    }
    assert_eq!(env["SWITCHYARD_AGENT"], "beta-one");
    assert_eq!(env["SWITCHYARD_BRANCH"], record["branch"].as_str().unwrap());
    assert_eq!(env["SWITCHYARD_DATABASE"], "switchyard_beta_one");
    assert_eq!(record["database"], "switchyard_beta_one");
    assert_eq!(
        env["SWITCHYARD_PORT_START"].parse::<i64>().unwrap(),
        record["port_start"].as_i64().unwrap()
    );
    assert_eq!(
        env["SWITCHYARD_PORT_END"].parse::<i64>().unwrap(),
        record["port_end"].as_i64().unwrap()
    );
    assert!(env["DATABASE_URL"].ends_with(&format!("/{}", record["database"].as_str().unwrap())));
    assert!(env["DATABASE_URL"].contains(&format!(":{}/", record["port_start"].as_i64().unwrap())));
}

// SY-2, one claim per agent

#[test]
fn second_claim_for_the_same_agent_fails_and_changes_nothing() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    let registry_before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let worktrees_before: Vec<String> = {
        let mut names: Vec<String> = std::fs::read_dir(stack.join("worktrees"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("alpha"), "{err}");
    assert_eq!(
        std::fs::read(stack.join(REGISTRY_NAME)).unwrap(),
        registry_before
    );
    let worktrees_after: Vec<String> = {
        let mut names: Vec<String> = std::fs::read_dir(stack.join("worktrees"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(worktrees_after, worktrees_before);
}

// SY-3, the port pool

#[test]
fn ranges_never_overlap_and_the_pool_can_run_out() {
    let base = temp_dir("pool");
    let small = init_stack_dir(&base, &[("range_count", 2)]);
    claim_ok(&small, "one");
    claim_ok(&small, "two");
    let data = read_registry(&small);
    let mut ranges: Vec<(i64, i64)> = data["claims"]
        .as_object()
        .unwrap()
        .values()
        .map(|record| {
            (
                record["port_start"].as_i64().unwrap(),
                record["port_end"].as_i64().unwrap(),
            )
        })
        .collect();
    ranges.sort();
    assert_eq!(ranges, vec![(20000, 20009), (20010, 20019)]);
    let (code, _, err) = run_cli(&["claim", "three", "--stack", &small.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("exhausted"), "{err}");
    let claims = read_registry(&small)["claims"].as_object().unwrap().clone();
    assert_eq!(claims.keys().collect::<Vec<_>>(), vec!["one", "two"]);
    assert!(!small.join("worktrees").join("three").exists());
}

// SY-4, atomicity under concurrency

fn race(stack: &Path, agents: &[&str]) -> Vec<(String, i32)> {
    let control = temp_dir("race");
    let go = control.join("go");
    let mut children = Vec::new();
    for agent in agents {
        let child = Command::new("sh")
            .arg("-c")
            .arg("while [ ! -e \"$0\" ]; do sleep 0.005; done; exec \"$SWITCHYARD_BIN\" claim \"$1\" --stack \"$2\"")
            .arg(&go)
            .arg(agent)
            .arg(stack)
            .env("SWITCHYARD_BIN", env!("CARGO_BIN_EXE_switchyard"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        children.push((agent.to_string(), child));
    }
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(&go, "go").unwrap();
    let mut results = Vec::new();
    for (agent, mut child) in children {
        let status = child.wait().unwrap();
        results.push((agent, status.code().unwrap_or(-1)));
    }
    results
}

#[test]
fn racing_claims_for_one_agent_produce_exactly_one_success() {
    let stack = stack_fixture();
    let results = race(&stack, &["alpha"; 6]);
    let mut codes: Vec<i32> = results.iter().map(|(_, code)| *code).collect();
    codes.sort();
    assert_eq!(codes, vec![0, 1, 1, 1, 1, 1]);
    let data = read_registry(&stack);
    assert_eq!(
        data["claims"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["alpha"]
    );
    let worktrees: Vec<String> = std::fs::read_dir(stack.join("worktrees"))
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                Some(entry.file_name().to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(worktrees, vec!["alpha".to_string()]);
}

#[test]
fn racing_claims_for_distinct_agents_never_overlap() {
    let base = temp_dir("race-distinct");
    let small = init_stack_dir(&base, &[("range_count", 3)]);
    let agents = ["racer-a", "racer-b", "racer-c", "racer-d", "racer-e"];
    let results = race(&small, &agents);
    let codes: Vec<i32> = results.iter().map(|(_, code)| *code).collect();
    assert_eq!(codes.iter().filter(|code| **code == 0).count(), 3);
    assert_eq!(codes.iter().filter(|code| **code == 1).count(), 2);
    let data = read_registry(&small);
    let claims = data["claims"].as_object().unwrap();
    assert_eq!(claims.len(), 3);
    let mut ranges: Vec<(i64, i64)> = claims
        .values()
        .map(|record| {
            (
                record["port_start"].as_i64().unwrap(),
                record["port_end"].as_i64().unwrap(),
            )
        })
        .collect();
    ranges.sort();
    for pair in ranges.windows(2) {
        assert!(pair[1].0 > pair[0].1);
    }
    let winners: std::collections::BTreeSet<String> = results
        .iter()
        .filter(|(_, code)| *code == 0)
        .map(|(agent, _)| agent.clone())
        .collect();
    let claimed: std::collections::BTreeSet<String> = claims.keys().cloned().collect();
    assert_eq!(claimed, winners);
    let worktrees: std::collections::BTreeSet<String> = std::fs::read_dir(small.join("worktrees"))
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                Some(entry.file_name().to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(worktrees, winners);
}

#[test]
fn twenty_processes_racing_a_four_range_pool_produce_exactly_four_successes() {
    let base = temp_dir("race-twenty");
    let small = init_stack_dir(&base, &[("range_count", 4)]);
    let agents: Vec<String> = (1..=20).map(|n| format!("racer-{n:02}")).collect();
    let agent_refs: Vec<&str> = agents.iter().map(|agent| agent.as_str()).collect();
    let results = race(&small, &agent_refs);
    let codes: Vec<i32> = results.iter().map(|(_, code)| *code).collect();
    assert_eq!(codes.iter().filter(|code| **code == 0).count(), 4);
    assert_eq!(codes.iter().filter(|code| **code == 1).count(), 16);
    let data = read_registry(&small);
    let claims = data["claims"].as_object().unwrap();
    assert_eq!(claims.len(), 4);
    let mut ranges: Vec<(i64, i64)> = claims
        .values()
        .map(|record| {
            (
                record["port_start"].as_i64().unwrap(),
                record["port_end"].as_i64().unwrap(),
            )
        })
        .collect();
    ranges.sort();
    for pair in ranges.windows(2) {
        assert!(pair[1].0 > pair[0].1);
    }
    for record in claims.values() {
        let start = record["port_start"].as_i64().unwrap();
        assert_eq!((start - 20000) % 10, 0);
        assert_eq!(record["port_end"].as_i64().unwrap(), start + 9);
    }
}

// SY-5, the list view

#[test]
fn list_prints_exactly_the_registry_claims() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    claim_ok(&stack, "beta");
    std::fs::create_dir(stack.join("worktrees").join("ghost")).unwrap();
    std::fs::write(stack.join("stray.txt"), "not a claim").unwrap();
    let (code, out, err) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("alpha"));
    assert!(out.contains("beta"));
    assert!(out.contains("switchyard_alpha"));
    assert!(out.contains("20010-20019"));
    assert!(!out.contains("ghost"));
    let data_rows: Vec<&str> = out
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("AGENT"))
        .collect();
    assert_eq!(data_rows.len(), 2);
    let (code, _, err) = run_cli(&["release", "beta", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let (code, out, _) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0);
    assert!(!out.contains("beta"));
    assert!(out.contains("alpha"));
}

// SY-6, complete release

#[test]
fn release_returns_every_resource_and_frees_the_range() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    let data = read_registry(&stack);
    let record = data["claims"]["alpha"].clone();
    let worktree = stack.join(record["worktree"].as_str().unwrap());
    let env_file = stack.join(record["env_file"].as_str().unwrap());
    std::fs::write(worktree.join("scratch.txt"), "agent scratch").unwrap();
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(!worktree.exists());
    assert!(!env_file.exists());
    assert_eq!(
        git(
            &stack.join("repo"),
            &["branch", "--list", "switchyard/alpha"]
        )
        .trim(),
        ""
    );
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
    claim_ok(&stack, "beta");
    let reused = read_registry(&stack)["claims"]["beta"].clone();
    assert_eq!(
        (
            reused["port_start"].as_i64().unwrap(),
            reused["port_end"].as_i64().unwrap()
        ),
        (
            record["port_start"].as_i64().unwrap(),
            record["port_end"].as_i64().unwrap()
        )
    );
}

// SY-7, unknown release

#[test]
fn release_of_an_unknown_agent_is_an_error_and_changes_nothing() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    let registry_before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "nobody", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("nobody"), "{err}");
    assert_eq!(
        std::fs::read(stack.join(REGISTRY_NAME)).unwrap(),
        registry_before
    );
    assert!(stack.join("worktrees").join("alpha").is_dir());
}

// SY-8, a corrupt registry

fn valid_record(agent: &str, port_start: i64) -> serde_json::Value {
    serde_json::json!({
        "agent": agent,
        "branch": switchyard::branch_name(agent),
        "worktree": format!("{WORKTREES_NAME}/{agent}"),
        "port_start": port_start,
        "port_end": port_start + 9,
        "database": switchyard::database_name(agent),
        "env_file": format!("{WORKTREES_NAME}/{agent}/{ENV_NAME}"),
        "claimed_at": "2026-11-02T00:00:00+00:00",
    })
}

fn registry_json(claims: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "port_pool": {"start": 20000, "range_size": 10, "range_count": 16},
        "claims": claims,
    })
}

fn corrupt_bodies() -> Vec<String> {
    let mut bodies = vec![
        "this is not json {".to_string(),
        serde_json::json!({"version": 99, "port_pool": {}, "claims": {}}).to_string(),
        serde_json::json!({
            "version": 1,
            "port_pool": {"start": 20000, "range_size": 10, "range_count": 4},
            "claims": ["alpha"],
        })
        .to_string(),
        registry_json(serde_json::json!({
            "alpha": valid_record("alpha", 20000),
            "beta": valid_record("beta", 20005),
        }))
        .to_string(),
    ];
    let mut version_float = registry_json(serde_json::json!({}));
    version_float["version"] = serde_json::json!(1.0);
    bodies.push(version_float.to_string());
    let mut version_bool = registry_json(serde_json::json!({}));
    version_bool["version"] = serde_json::json!(true);
    bodies.push(version_bool.to_string());
    let mut surprise = registry_json(serde_json::json!({}));
    surprise["surprise"] = serde_json::json!("an unrecorded field");
    bodies.push(surprise.to_string());
    let mut wide_pool = registry_json(serde_json::json!({}));
    wide_pool["port_pool"] =
        serde_json::json!({"start": 20000, "range_size": 10, "range_count": 16, "extra": 1});
    bodies.push(wide_pool.to_string());
    let mut extra_record_field = valid_record("alpha", 20000);
    extra_record_field["note"] = serde_json::json!("not a recorded field");
    bodies.push(registry_json(serde_json::json!({"alpha": extra_record_field})).to_string());
    let mut missing_claimed_at = valid_record("alpha", 20000);
    missing_claimed_at
        .as_object_mut()
        .unwrap()
        .remove("claimed_at");
    bodies.push(registry_json(serde_json::json!({"alpha": missing_claimed_at})).to_string());
    let mut forged_branch = valid_record("alpha", 20000);
    forged_branch["branch"] = serde_json::json!("main");
    bodies.push(registry_json(serde_json::json!({"alpha": forged_branch})).to_string());
    let mut forged_worktree = valid_record("alpha", 20000);
    forged_worktree["worktree"] = serde_json::json!("worktrees/beta");
    forged_worktree["env_file"] = serde_json::json!(format!("worktrees/beta/{ENV_NAME}"));
    bodies.push(registry_json(serde_json::json!({"alpha": forged_worktree})).to_string());
    bodies.push(
        registry_json(serde_json::json!({"../escape": valid_record("../escape", 20000)}))
            .to_string(),
    );
    let mut past_end = registry_json(serde_json::json!({}));
    past_end["port_pool"] =
        serde_json::json!({"start": 65000, "range_size": 100, "range_count": 16});
    bodies.push(past_end.to_string());
    bodies.push(
        registry_json(serde_json::json!({"alpha": valid_record("alpha", 20005)})).to_string(),
    );
    let mut wrong_size = valid_record("alpha", 20000);
    wrong_size["port_end"] = serde_json::json!(20004);
    bodies.push(registry_json(serde_json::json!({"alpha": wrong_size})).to_string());
    let mut bad_stamp = valid_record("alpha", 20000);
    bad_stamp["claimed_at"] = serde_json::json!("not a timestamp");
    bodies.push(registry_json(serde_json::json!({"alpha": bad_stamp})).to_string());
    bodies.push(
        r#"{"version": 1, "version": 1, "port_pool": {"start": 20000, "range_size": 10, "range_count": 16}, "claims": {}}"#
            .to_string(),
    );
    bodies.push(
        r#"{"version": 1, "port_pool": {"start": 20000, "range_size": 10, "range_count": 16}, "claims": {"alpha": {"agent": "alpha", "branch": "switchyard/alpha", "branch": "switchyard/alpha", "worktree": "worktrees/alpha", "port_start": 20000, "port_end": 20009, "database": "switchyard_alpha", "env_file": "worktrees/alpha/.env.switchyard", "claimed_at": "2026-11-02T00:00:00+00:00"}}}"#
            .to_string(),
    );
    bodies
}

#[test]
fn a_corrupt_registry_fails_every_command_and_changes_nothing() {
    for body in corrupt_bodies() {
        let stack = stack_fixture();
        std::fs::write(stack.join(REGISTRY_NAME), &body).unwrap();
        let before = snapshot(&stack);
        let stack_text = stack.to_string_lossy().into_owned();
        for argv in [
            vec!["claim", "alpha", "--stack", &stack_text],
            vec!["list", "--stack", &stack_text],
            vec!["release", "alpha", "--stack", &stack_text],
        ] {
            let (code, _, err) = run_cli(&argv);
            assert_ne!(code, 0, "{body}");
            assert!(err.contains("corrupt"), "{body} gave {err}");
        }
        assert_eq!(snapshot(&stack), before, "{body}");
    }
}

// SY-9, an uninitialised stack

#[test]
fn an_uninitialised_stack_fails_every_command_and_creates_nothing() {
    let base = temp_dir("uninitialised");
    let empty = base.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let missing = base.join("missing");
    for target in [&empty, &missing] {
        let target_text = target.to_string_lossy().into_owned();
        for argv in [
            vec!["claim", "alpha", "--stack", &target_text],
            vec!["list", "--stack", &target_text],
            vec!["release", "alpha", "--stack", &target_text],
        ] {
            let (code, _, err) = run_cli(&argv);
            assert_ne!(code, 0);
            assert!(!err.trim().is_empty());
        }
    }
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    assert!(!missing.exists());
}

// SY-10, agent name safety

#[test]
fn unsafe_agent_names_are_rejected_and_change_nothing() {
    for name in [
        "../escape",
        "a/b",
        "two words",
        "Upper",
        ".hidden",
        "-lead",
        "",
    ] {
        let stack = stack_fixture();
        let before = snapshot(&stack);
        let (code, _, err) = run_cli(&["claim", name, "--stack", &stack.to_string_lossy()]);
        assert_ne!(code, 0, "{name}");
        assert!(!err.trim().is_empty());
        assert_eq!(snapshot(&stack), before, "{name}");
        assert!(!stack.parent().unwrap().join("escape").exists());
    }
}

// SY-8, forged records, decode damage, and the repository perimeter

#[test]
fn a_record_pointing_at_another_agents_worktree_is_corrupt() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    claim_ok(&stack, "beta");
    let mut data = read_registry(&stack);
    data["claims"]["alpha"]["worktree"] = serde_json::json!("worktrees/beta");
    data["claims"]["alpha"]["env_file"] = serde_json::json!(format!("worktrees/beta/{ENV_NAME}"));
    std::fs::write(
        stack.join(REGISTRY_NAME),
        serde_json::to_string(&data).unwrap(),
    )
    .unwrap();
    let before = snapshot(&stack);
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("corrupt"), "{err}");
    assert_eq!(snapshot(&stack), before);
    assert!(stack.join("worktrees").join("beta").is_dir());
}

#[test]
fn a_registry_that_is_not_utf8_text_is_corrupt_and_changes_nothing() {
    let utf16_text = "{\"version\": 1, \"port_pool\": {}, \"claims\": {}}";
    let mut utf16_blob = vec![0xFF, 0xFE];
    for unit in utf16_text.encode_utf16() {
        utf16_blob.extend_from_slice(&unit.to_le_bytes());
    }
    for blob in [
        b"\xff\xfe\x00 not a registry in any encoding".to_vec(),
        utf16_blob,
    ] {
        let stack = stack_fixture();
        std::fs::write(stack.join(REGISTRY_NAME), &blob).unwrap();
        let before = snapshot(&stack);
        let stack_text = stack.to_string_lossy().into_owned();
        for argv in [
            vec!["claim", "alpha", "--stack", &stack_text],
            vec!["list", "--stack", &stack_text],
            vec!["release", "alpha", "--stack", &stack_text],
        ] {
            let (code, _, err) = run_cli(&argv);
            assert_ne!(code, 0);
            assert!(err.contains("corrupt"), "{err}");
        }
        assert_eq!(snapshot(&stack), before);
    }
}

#[test]
fn a_registry_path_that_is_a_directory_is_corrupt_not_missing() {
    let stack = stack_fixture();
    let registry = stack.join(REGISTRY_NAME);
    std::fs::remove_file(&registry).unwrap();
    std::fs::create_dir(&registry).unwrap();
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "alpha", "--stack", &stack_text],
        vec!["list", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_ne!(code, 0);
        assert!(err.contains("corrupt"), "{err}");
        assert!(err.contains("not a file"), "{err}");
    }
}

#[test]
fn a_lock_path_that_is_a_directory_is_a_clean_error() {
    let stack = stack_fixture();
    let lock = stack.join(LOCK_NAME);
    std::fs::remove_file(&lock).unwrap();
    std::fs::create_dir(&lock).unwrap();
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "alpha", "--stack", &stack_text],
        vec!["release", "alpha", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_ne!(code, 0);
        assert!(err.contains("lock"), "{err}");
    }
}

#[test]
fn a_missing_lock_file_is_recreated_when_the_registry_survives() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    std::fs::remove_file(stack.join(LOCK_NAME)).unwrap();
    claim_ok(&stack, "beta");
    assert!(stack.join(LOCK_NAME).is_file());
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let data = read_registry(&stack);
    assert_eq!(
        data["claims"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["beta"]
    );
}

#[test]
fn a_corrupt_registry_with_no_lock_file_creates_no_lock_file() {
    let stack = stack_fixture();
    std::fs::write(stack.join(REGISTRY_NAME), "this is not json").unwrap();
    std::fs::remove_file(stack.join(LOCK_NAME)).unwrap();
    let before = snapshot(&stack);
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "alpha", "--stack", &stack_text],
        vec!["list", "--stack", &stack_text],
        vec!["release", "alpha", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_eq!(code, 2);
        assert!(err.contains("corrupt"), "{err}");
    }
    assert!(!stack.join(LOCK_NAME).exists());
    assert_eq!(snapshot(&stack), before);
}

#[test]
fn claim_refuses_a_stack_repo_that_lost_its_git_inside_an_outer_repo() {
    let base = temp_dir("outer");
    git_in(&base, &["init", "-q", "-b", "main", "outer"]);
    let outer = base.join("outer");
    let stack = init_stack_dir(&outer, &[]);
    claim_ok(&stack, "alpha");
    let registry_before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    std::fs::remove_dir_all(stack.join("repo").join(".git")).unwrap();
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "beta", "--stack", &stack_text],
        vec!["release", "alpha", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_ne!(code, 0);
        assert!(err.contains(".git"), "{err}");
    }
    assert_eq!(
        std::fs::read(stack.join(REGISTRY_NAME)).unwrap(),
        registry_before
    );
    assert!(!git(&outer, &["branch", "--list"]).contains("switchyard"));
    let resolved = switchyard::resolve_path(&stack);
    assert!(!git(&outer, &["worktree", "list"]).contains(&resolved.to_string_lossy().into_owned()));
}

// SY-1 and SY-9, claim failure completeness and stack path handling

#[test]
fn a_blocked_claim_leaves_no_branch_and_the_name_can_be_claimed() {
    let stack = stack_fixture();
    let blocker = stack.join("worktrees").join("alpha");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("junk.txt"), "in the way").unwrap();
    let (code, _, _) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert_eq!(
        git(
            &stack.join("repo"),
            &["branch", "--list", "switchyard/alpha"]
        )
        .trim(),
        ""
    );
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
    std::fs::remove_dir_all(&blocker).unwrap();
    claim_ok(&stack, "alpha");
    assert!(stack.join("worktrees").join("alpha").is_dir());
}

#[test]
fn a_failed_registry_save_leaves_no_temporary_file() {
    let stack = stack_fixture();
    let data = switchyard::load_registry(&stack).unwrap();
    // Stand a directory where the registry file belongs, so the
    // temporary write lands and the rename fails against a real
    // file system, the shape the Python suite staged by patching
    // the rename.
    let registry = stack.join(REGISTRY_NAME);
    std::fs::remove_file(&registry).unwrap();
    std::fs::create_dir(&registry).unwrap();
    let error = switchyard::save_registry(&stack, &data)
        .expect_err("the save must fail when the rename cannot land");
    assert!(
        error.message().contains("cannot be written"),
        "{}",
        error.message()
    );
    assert!(!stack.join(format!("{REGISTRY_NAME}.tmp")).exists());
    assert!(registry.is_dir());
}

#[test]
fn a_registry_save_replaces_the_path_and_never_writes_through_a_link() {
    let stack = stack_fixture();
    let data = switchyard::load_registry(&stack).unwrap();
    // The registry is saved by rename from a sibling temporary
    // file, so a save replaces the registry path itself and never
    // writes through it. Stand the registry path as a symlink to a
    // victim file and watch both sides of the save.
    let registry = stack.join(REGISTRY_NAME);
    let victim = stack.join("victim.txt");
    std::fs::write(&victim, "sentinel").unwrap();
    std::fs::remove_file(&registry).unwrap();
    std::os::unix::fs::symlink(&victim, &registry).unwrap();
    switchyard::save_registry(&stack, &data).unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "sentinel");
    let meta = std::fs::symlink_metadata(&registry).unwrap();
    assert!(!meta.file_type().is_symlink());
    assert_eq!(
        switchyard::load_registry(&stack).unwrap().claims.len(),
        data.claims.len()
    );
    assert!(!stack.join(format!("{REGISTRY_NAME}.tmp")).exists());
}

#[test]
fn claim_against_a_repository_with_no_commits_fails_and_records_nothing() {
    let stack = stack_fixture();
    std::fs::remove_dir_all(stack.join("repo")).unwrap();
    git_in(&stack, &["init", "-q", "-b", "main", "repo"]);
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("no commits"), "{err}");
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
    assert!(!stack.join("worktrees").join("alpha").exists());
}

#[test]
fn a_relative_stack_path_works_end_to_end() {
    let base = temp_dir("relative");
    let (code, _, err) = run_cli_full(&["init", "--stack", "stack"], &[], Some(&base));
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli_full(&["claim", "alpha", "--stack", "stack"], &[], Some(&base));
    assert_eq!(code, 0, "{err}");
    let stack = base.join("stack");
    assert!(stack.join("worktrees").join("alpha").is_dir());
    assert!(!stack.join("repo").join("stack").exists());
    let (code, out, err) = run_cli_full(&["list", "--stack", "stack"], &[], Some(&base));
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("alpha"));
    let (code, _, err) = run_cli_full(&["release", "alpha", "--stack", "stack"], &[], Some(&base));
    assert_eq!(code, 0, "{err}");
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
}

#[test]
fn a_stack_root_that_is_a_file_fails_every_command_cleanly() {
    let base = temp_dir("file-stack");
    let target = base.join("stack");
    std::fs::write(&target, "a file, not a stack").unwrap();
    let target_text = target.to_string_lossy().into_owned();
    for argv in [
        vec!["init", "--stack", &target_text],
        vec!["claim", "alpha", "--stack", &target_text],
        vec!["list", "--stack", &target_text],
        vec!["release", "alpha", "--stack", &target_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_ne!(code, 0);
        assert!(!err.trim().is_empty());
    }
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "a file, not a stack"
    );
}

#[test]
fn a_failed_init_leaves_no_half_initialised_stack() {
    let base = temp_dir("failed-init");
    let stack = base.join("stack");
    std::fs::create_dir(&stack).unwrap();
    std::fs::write(stack.join("repo"), "a file where the repo belongs").unwrap();
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(!err.trim().is_empty());
    let names: Vec<String> = std::fs::read_dir(&stack)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["repo".to_string()]);
    assert_eq!(
        std::fs::read_to_string(stack.join("repo")).unwrap(),
        "a file where the repo belongs"
    );
    let (code, _, err) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("no registry"), "{err}");
}

#[test]
fn init_refuses_a_foreign_repository_at_the_repo_path() {
    let base = temp_dir("foreign-repo");
    let stack = base.join("stack");
    let repo = stack.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "foreign@example.invalid"]);
    git(&repo, &["config", "user.name", "foreign owner"]);
    std::fs::write(repo.join("README.md"), "the foreign readme\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "foreign commit"]);
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let before = snapshot(&stack);
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains(&repo.to_string_lossy().into_owned()), "{err}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        git(&repo, &["config", "user.email"]).trim(),
        "foreign@example.invalid"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "the foreign readme\n"
    );
    assert_eq!(snapshot(&stack), before);
}

#[test]
fn init_refuses_a_plain_file_at_the_repo_path() {
    let base = temp_dir("file-repo");
    let stack = base.join("stack");
    std::fs::create_dir(&stack).unwrap();
    let repo = stack.join("repo");
    std::fs::write(&repo, "a file where the repo belongs").unwrap();
    let before = snapshot(&stack);
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains(&repo.to_string_lossy().into_owned()), "{err}");
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(snapshot(&stack), before);
}

#[test]
fn init_refuses_a_plain_file_at_the_worktrees_path() {
    let base = temp_dir("file-worktrees");
    let stack = base.join("stack");
    std::fs::create_dir(&stack).unwrap();
    std::fs::write(
        stack.join(WORKTREES_NAME),
        "a file where the worktrees belong",
    )
    .unwrap();
    let before = snapshot(&stack);
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(snapshot(&stack), before);
}

#[test]
fn init_into_a_pre_created_empty_stack_root_succeeds() {
    let base = temp_dir("pre-created");
    let stack = base.join("stack");
    std::fs::create_dir(&stack).unwrap();
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(stack.join(REGISTRY_NAME).is_file());
    assert!(stack.join("repo").join(".git").exists());
}

// SY-10, the name length cap

#[test]
fn the_agent_name_cap_is_forty_characters() {
    let stack = stack_fixture();
    let long_name = "a".repeat(41);
    let (code, _, err) = run_cli(&["claim", &long_name, "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert!(err.contains("40"), "{err}");
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
    let max_name = "a".repeat(40);
    let (code, _, err) = run_cli(&["claim", &max_name, "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
}

// SY-10, names carrying newlines, and the inherited git environment

#[test]
fn agent_names_with_newlines_are_rejected_and_change_nothing() {
    for name in ["alpha\n", "al\npha"] {
        let stack = stack_fixture();
        let before = snapshot(&stack);
        let (code, _, err) = run_cli(&["claim", name, "--stack", &stack.to_string_lossy()]);
        assert_ne!(code, 0);
        assert!(err.contains("invalid agent name"), "{err}");
        assert_eq!(snapshot(&stack), before);
    }
}

#[test]
fn a_claim_ignores_git_location_variables_from_the_caller() {
    let base = temp_dir("git-env");
    let stack = init_stack_dir(&base, &[]);
    let foreign = base.join("foreign");
    git_in(&base, &["init", "-q", "-b", "main", "foreign"]);
    git(
        &foreign,
        &["config", "user.email", "foreign@example.invalid"],
    );
    git(&foreign, &["config", "user.name", "foreign"]);
    std::fs::write(foreign.join("seed.txt"), "seed").unwrap();
    git(&foreign, &["add", "seed.txt"]);
    git(
        &foreign,
        &["commit", "-q", "-m", "Seed the foreign repository"],
    );
    let branches_before = git(&foreign, &["branch", "--list"]);
    let foreign_git = foreign.join(".git").to_string_lossy().into_owned();
    let foreign_text = foreign.to_string_lossy().into_owned();
    let (code, _, err) = run_cli_full(
        &["claim", "alpha", "--stack", &stack.to_string_lossy()],
        &[("GIT_DIR", &foreign_git), ("GIT_WORK_TREE", &foreign_text)],
        None,
    );
    assert_eq!(code, 0, "{err}");
    assert!(!git(
        &stack.join("repo"),
        &["branch", "--list", "switchyard/alpha"]
    )
    .trim()
    .is_empty());
    assert_eq!(
        read_registry(&stack)["claims"]["alpha"]["branch"],
        "switchyard/alpha"
    );
    assert_eq!(git(&foreign, &["branch", "--list"]), branches_before);
    assert!(!foreign.join("worktrees").exists());
}

// The third repair, rollback ownership, the linked worktree
// perimeter, and decode depth

fn handmade_branch(stack: &Path, base: &Path) -> String {
    let repo = stack.join("repo");
    git(&repo, &["branch", "switchyard/alpha"]);
    let scratch = base.join("handmade");
    git(
        &repo,
        &[
            "worktree",
            "add",
            &scratch.to_string_lossy(),
            "switchyard/alpha",
        ],
    );
    std::fs::write(scratch.join("hand.txt"), "hand made work").unwrap();
    git(&scratch, &["add", "hand.txt"]);
    git(&scratch, &["commit", "-q", "-m", "Hand made commit"]);
    let sha = git(&repo, &["rev-parse", "switchyard/alpha"])
        .trim()
        .to_string();
    git(
        &repo,
        &["worktree", "remove", "--force", &scratch.to_string_lossy()],
    );
    sha
}

#[test]
fn a_preexisting_branch_survives_a_plain_failed_claim() {
    let base = temp_dir("preexisting-plain");
    let stack = init_stack_dir(&base, &[]);
    let sha = handmade_branch(&stack, &base);
    let (code, _, _) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert_eq!(
        git(&stack.join("repo"), &["rev-parse", "switchyard/alpha"]).trim(),
        sha
    );
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
}

#[test]
fn a_preexisting_branch_survives_an_obstructed_failed_claim() {
    let base = temp_dir("preexisting-obstructed");
    let stack = init_stack_dir(&base, &[]);
    let sha = handmade_branch(&stack, &base);
    let blocker = stack.join("worktrees").join("alpha");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("junk.txt"), "in the way").unwrap();
    let (code, _, _) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_ne!(code, 0);
    assert_eq!(
        git(&stack.join("repo"), &["rev-parse", "switchyard/alpha"]).trim(),
        sha
    );
    assert_eq!(
        std::fs::read_to_string(blocker.join("junk.txt")).unwrap(),
        "in the way"
    );
    assert_eq!(read_registry(&stack)["claims"], serde_json::json!({}));
}

#[test]
fn claim_and_release_refuse_a_linked_worktree_of_a_foreign_repo() {
    let base = temp_dir("linked");
    git_in(&base, &["init", "-q", "-b", "main", "foreign"]);
    let foreign = base.join("foreign");
    git(
        &foreign,
        &["config", "user.email", "foreign@example.invalid"],
    );
    git(&foreign, &["config", "user.name", "foreign"]);
    std::fs::write(foreign.join("seed.txt"), "seed").unwrap();
    git(&foreign, &["add", "seed.txt"]);
    git(
        &foreign,
        &["commit", "-q", "-m", "Seed the foreign repository"],
    );
    let stack = init_stack_dir(&base, &[]);
    std::fs::remove_dir_all(stack.join("repo")).unwrap();
    git(
        &foreign,
        &["worktree", "add", &stack.join("repo").to_string_lossy()],
    );
    let branches_before = git(&foreign, &["branch", "--list"]);
    let worktrees_before = git(&foreign, &["worktree", "list"]);
    let registry_before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "alpha", "--stack", &stack_text],
        vec!["release", "alpha", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_ne!(code, 0);
        assert!(err.contains("will not run git"), "{err}");
    }
    assert_eq!(git(&foreign, &["branch", "--list"]), branches_before);
    assert_eq!(git(&foreign, &["worktree", "list"]), worktrees_before);
    assert_eq!(
        std::fs::read(stack.join(REGISTRY_NAME)).unwrap(),
        registry_before
    );
}

#[test]
fn an_extremely_nested_registry_is_corrupt_not_a_traceback() {
    let stack = stack_fixture();
    let body = "[".repeat(20_000) + &"]".repeat(20_000);
    std::fs::write(stack.join(REGISTRY_NAME), body).unwrap();
    let before = snapshot(&stack);
    let stack_text = stack.to_string_lossy().into_owned();
    for argv in [
        vec!["claim", "alpha", "--stack", &stack_text],
        vec!["list", "--stack", &stack_text],
        vec!["release", "alpha", "--stack", &stack_text],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_eq!(code, 2);
        assert!(err.contains("corrupt"), "{err}");
        assert!(!err.contains("Traceback"), "{err}");
    }
    assert_eq!(snapshot(&stack), before);
}

// The fourth repair, a failed env file write is a clean error

#[test]
fn a_failed_env_file_write_is_a_clean_error_and_rolls_back() {
    let stack = stack_fixture();
    let repo = stack.join("repo");
    let planted = repo.join(ENV_NAME);
    std::fs::create_dir(&planted).unwrap();
    std::fs::write(planted.join("keep.txt"), "tracked placeholder").unwrap();
    git(&repo, &["add", ENV_NAME]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "Plant a directory at the env file path",
        ],
    );
    let registry_before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 1);
    assert!(err.contains("error:"), "{err}");
    assert!(err.contains("env file"), "{err}");
    assert!(err.contains("cannot be written"), "{err}");
    assert!(!err.contains("Traceback"), "{err}");
    assert_eq!(
        std::fs::read(stack.join(REGISTRY_NAME)).unwrap(),
        registry_before
    );
    assert_eq!(
        git(&repo, &["branch", "--list", "switchyard/alpha"]).trim(),
        ""
    );
    assert!(!git(&repo, &["worktree", "list"]).contains("worktrees/alpha"));
    assert!(!stack.join("worktrees").join("alpha").exists());
}

// The module entry point

#[test]
fn the_module_entry_point_lists_a_live_claim() {
    let stack = stack_fixture();
    claim_ok(&stack, "alpha");
    let output = Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args(["list", "--stack", &stack.to_string_lossy()])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("switchyard_alpha"));
}

// Registry fixtures written by the Python implementation

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn blank_stamps(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(start) = line.find("\"claimed_at\": \"") {
            let value_start = start + "\"claimed_at\": \"".len();
            if let Some(end_offset) = line[value_start..].find('"') {
                out.push_str(&line[..value_start]);
                out.push_str("STAMP");
                out.push_str(&line[value_start + end_offset..]);
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

#[test]
fn python_written_empty_registry_lists_no_claims() {
    let base = temp_dir("fx-empty");
    let stack = base.join("stack");
    std::fs::create_dir_all(&stack).unwrap();
    std::fs::write(
        stack.join(REGISTRY_NAME),
        fixture("python_registry_empty.json"),
    )
    .unwrap();
    let data = switchyard::load_registry(&stack).unwrap();
    assert_eq!(data.port_pool.range_count, 16);
    assert!(data.claims.is_empty());
    let (code, out, err) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("AGENT"));
    let rows: Vec<&str> = out
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("AGENT"))
        .collect();
    assert!(rows.is_empty());
}

#[test]
fn python_written_partial_registry_loads_and_the_rust_claim_matches_python_bytes() {
    let base = temp_dir("fx-partial");
    let stack = init_stack_dir(&base, &[("range_count", 4)]);
    std::fs::write(
        stack.join(REGISTRY_NAME),
        fixture("python_registry_partial.json"),
    )
    .unwrap();
    let data = switchyard::load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 2);
    let (code, out, err) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("alpha") && out.contains("beta"));
    assert!(out.contains("20000-20009") && out.contains("20010-20019"));
    claim_ok(&stack, "gamma");
    let written = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    let expected = String::from_utf8(fixture("python_registry_after_claim.json")).unwrap();
    assert_eq!(blank_stamps(&written), blank_stamps(&expected));
    assert_eq!(
        read_registry(&stack)["claims"]["gamma"]["port_start"],
        20020
    );
}

#[test]
fn python_written_partial_registry_release_matches_python_bytes() {
    let base = temp_dir("fx-release");
    let stack = init_stack_dir(&base, &[("range_count", 4)]);
    std::fs::write(
        stack.join(REGISTRY_NAME),
        fixture("python_registry_partial.json"),
    )
    .unwrap();
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let written = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    let expected = String::from_utf8(fixture("python_registry_released.json")).unwrap();
    assert_eq!(blank_stamps(&written), blank_stamps(&expected));
}

#[test]
fn python_written_released_registry_claim_reuses_the_freed_slot_and_matches_python_bytes() {
    let base = temp_dir("fx-reclaim");
    let stack = init_stack_dir(&base, &[("range_count", 4)]);
    std::fs::write(
        stack.join(REGISTRY_NAME),
        fixture("python_registry_released.json"),
    )
    .unwrap();
    claim_ok(&stack, "delta");
    let data = read_registry(&stack);
    assert_eq!(data["claims"]["delta"]["port_start"], 20000);
    assert_eq!(data["claims"]["delta"]["port_end"], 20009);
    let written = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    let expected = String::from_utf8(fixture("python_registry_after_reclaim.json")).unwrap();
    assert_eq!(blank_stamps(&written), blank_stamps(&expected));
}

// The port repair pins. The pool arithmetic overflow, the planted
// temporary name, and the counts behind them come from the three
// reviews of the port head, and the overflow shapes are the red
// team's own drives.

#[test]
fn an_init_whose_pool_product_overflows_is_refused_and_creates_nothing() {
    let base = temp_dir("overflow-product");
    let stack = base.join("stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack.to_string_lossy(),
        "--range-size",
        "4611686018427387904",
        "--range-count",
        "4",
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    assert!(!stack.exists(), "a refused init leaves no stack behind");
}

#[test]
fn an_init_with_a_26_digit_port_start_is_refused_and_creates_nothing() {
    let base = temp_dir("overflow-start");
    let stack = base.join("stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack.to_string_lossy(),
        "--port-start",
        "99999999999999999999999999",
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    assert!(!stack.exists(), "a refused init leaves no stack behind");
}

#[test]
fn an_init_with_a_50_digit_port_start_is_refused_and_creates_nothing() {
    let base = temp_dir("overflow-saturate");
    let stack = base.join("stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack.to_string_lossy(),
        "--port-start",
        "99999999999999999999999999999999999999999999999999",
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    assert!(!stack.exists(), "a refused init leaves no stack behind");
}

#[test]
fn a_registry_whose_pool_product_overflows_reads_corrupt_and_changes_nothing() {
    let stack = stack_fixture();
    let path = stack.join(REGISTRY_NAME);
    let body = serde_json::json!({
        "version": 1,
        "port_pool": {"start": 20000, "range_size": 4611686018427387904i64, "range_count": 4},
        "claims": {}
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap() + "\n").unwrap();
    let planted = std::fs::read(&path).unwrap();
    let stack_arg = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["list", "--stack", &stack_arg]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_arg]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), planted);
}

#[test]
fn a_registry_whose_pool_end_addition_overflows_reads_corrupt() {
    let stack = stack_fixture();
    let path = stack.join(REGISTRY_NAME);
    let body = serde_json::json!({
        "version": 1,
        "port_pool": {"start": 9223372036854775797i64, "range_size": 100, "range_count": 100},
        "claims": {}
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap() + "\n").unwrap();
    let planted = std::fs::read(&path).unwrap();
    let (code, _, err) = run_cli(&["list", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains("the port pool reaches past port 65535"),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), planted);
}

#[test]
fn a_registry_save_never_writes_through_a_link_at_the_retired_tmp_name() {
    let stack = stack_fixture();
    let victim = stack.join("victim.txt");
    std::fs::write(&victim, "sentinel\n").unwrap();
    let retired = stack.join(format!("{REGISTRY_NAME}.tmp"));
    std::os::unix::fs::symlink(&victim, &retired).unwrap();
    switchyard::claim(&stack, "alpha").unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"sentinel\n");
    let registry_meta = std::fs::symlink_metadata(stack.join(REGISTRY_NAME)).unwrap();
    assert!(registry_meta.is_file() && !registry_meta.file_type().is_symlink());
    let data = read_registry(&stack);
    assert_eq!(data["claims"]["alpha"]["port_start"], 20000);
    // The plant itself is left standing, and no save residue of
    // any temporary shape remains in the stack.
    let retired_meta = std::fs::symlink_metadata(&retired).unwrap();
    assert!(retired_meta.file_type().is_symlink());
    for entry in std::fs::read_dir(&stack).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".tmp-"), "{name}");
    }
}
