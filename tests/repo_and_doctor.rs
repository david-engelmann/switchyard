//! Discriminating integration tests for slice SY-S2 obligations
//! SY-27 (`init --from`, `--branch`, and `switchyard-config.json`),
//! SY-28 (`release --keep-branch` and `--safe`), and SY-29 (`doctor`
//! and `doctor --fix`).
//!
//! Every fixture is synthetic and runs completely offline inside
//! temporary directories.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use switchyard::{
    CONFIG_NAME, EMPTY_HOOKS_NAME, ENV_NAME, LOCK_NAME, REGISTRY_NAME, REPO_NAME, ROUTING_LOG_NAME,
    WORKTREES_NAME,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> TempDir {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "switchyard-s2-test-{}-{n}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn shell_single_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn run_cli(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "git {:?} in {} failed: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git_code(cwd: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap()
        .status
        .code()
        .unwrap_or(-1)
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
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
            let meta = path.symlink_metadata().unwrap();
            if meta.is_dir() && !meta.file_type().is_symlink() {
                out.insert(rel, None);
                walk(&path, root, out);
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(
                    rel,
                    Some(target.to_string_lossy().into_owned().into_bytes()),
                );
            } else {
                out.insert(rel, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Build a synthetic local source git repository on branch `trunk`
/// with a second branch `feature-base`, a pre-existing
/// `switchyard/leftover` branch, a `remote.origin` entry, and an
/// executable `post-checkout` hook writing to `hook_marker`.
fn build_source_repo(base: &Path) -> (PathBuf, PathBuf) {
    let src = base.join("source-repo");
    let hook_marker = base.join("hook-executed.marker");
    fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "trunk"]);
    git(&src, &["config", "user.email", "synthetic@example.invalid"]);
    git(&src, &["config", "user.name", "synthetic"]);
    fs::write(src.join("README.md"), "Synthetic trunk readme.\n").unwrap();
    git(&src, &["add", "README.md"]);
    git(&src, &["commit", "-q", "-m", "Initial trunk commit"]);

    git(&src, &["checkout", "-q", "-b", "feature-base"]);
    fs::write(src.join("feature.txt"), "Synthetic feature payload.\n").unwrap();
    git(&src, &["add", "feature.txt"]);
    git(&src, &["commit", "-q", "-m", "Add feature payload"]);

    git(&src, &["branch", "switchyard/leftover"]);
    git(
        &src,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/upstream.git",
        ],
    );
    git(&src, &["checkout", "-q", "trunk"]);

    let hooks_dir = src.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let quoted_marker = shell_single_quote(&hook_marker);
    for hook_name in ["post-checkout", "reference-transaction"] {
        let hook = hooks_dir.join(hook_name);
        fs::write(
            &hook,
            format!("#!/bin/sh\necho {hook_name} >> {quoted_marker}\n"),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Remove any marker written while preparing `src` itself so the
    // test measures only what `switchyard` triggers from this point.
    let _ = fs::remove_file(&hook_marker);
    (src, hook_marker)
}

// =========================================================================
// Obligation SY-27: bare init compatibility, init --from, --branch, config
// =========================================================================

#[test]
fn bare_init_writes_no_config_and_keeps_synthetic_main_layout() {
    let base = temp_dir("bare-init");
    let stack = base.join("stack");
    let (code, _, err) = run_cli(&["init", "--stack", &stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(!stack.join(CONFIG_NAME).exists());
    let mut entries: Vec<String> = fs::read_dir(&stack)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            REPO_NAME.to_string(),
            REGISTRY_NAME.to_string(),
            LOCK_NAME.to_string(),
            WORKTREES_NAME.to_string(),
        ]
    );
    assert_eq!(
        git(&stack.join(REPO_NAME), &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}

#[test]
fn init_from_local_repo_clones_offline_detaches_head_strips_remotes_and_neutralises_hooks() {
    let base = temp_dir("init-from-default");
    let (src, hook_marker) = build_source_repo(&base);
    let trunk_sha = git(&src, &["rev-parse", "refs/heads/trunk"])
        .trim()
        .to_string();
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();

    let (code, out, err) = run_cli(&["init", "--stack", &stack_str, "--from", &src_str]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("initialised stack at"), "{out}");
    assert!(!hook_marker.exists(), "source hook must never execute");

    let cfg_raw = fs::read_to_string(stack.join(CONFIG_NAME)).unwrap();
    let cfg: serde_json::Value = serde_json::from_str(&cfg_raw).unwrap();
    let keys: Vec<&str> = cfg
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "base_branch",
            "env_template",
            "probe_ports",
            "seed_files",
            "source_repo",
            "version",
        ]
    );
    assert_eq!(cfg["base_branch"], "trunk");
    assert_eq!(cfg["env_template"], serde_json::Value::Null);
    assert_eq!(cfg["probe_ports"], false);
    assert_eq!(cfg["seed_files"], serde_json::json!([]));
    assert_eq!(
        cfg["source_repo"].as_str().unwrap(),
        fs::canonicalize(&src).unwrap().to_str().unwrap()
    );
    assert_eq!(cfg["version"], 1);

    let stack_repo = stack.join(REPO_NAME);
    // Detached HEAD at trunk.
    assert_ne!(
        git_code(&stack_repo, &["symbolic-ref", "--quiet", "HEAD"]),
        0
    );
    assert_eq!(git(&stack_repo, &["rev-parse", "HEAD"]).trim(), trunk_sha);
    // No origin remote, no refs/remotes/*, and no pre-existing switchyard/* branch.
    assert_eq!(git(&stack_repo, &["remote"]).trim(), "");
    assert_eq!(
        git(
            &stack_repo,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/"]
        )
        .trim(),
        ""
    );
    assert_eq!(
        git(
            &stack_repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/switchyard/"
            ]
        )
        .trim(),
        ""
    );

    // Empty hooks directory configured by absolute canonical path.
    let empty_hooks = stack_repo.join(EMPTY_HOOKS_NAME);
    assert!(empty_hooks.is_dir());
    let configured_hooks = git(&stack_repo, &["config", "--get", "core.hooksPath"]);
    assert_eq!(
        Path::new(configured_hooks.trim()),
        fs::canonicalize(&empty_hooks).unwrap()
    );

    // Plant an executable hook inside stack_repo/.git/hooks AND inside the agent worktree's
    // relative .switchyard-empty-hooks path to prove neither executes!
    fs::create_dir_all(stack_repo.join(".git/hooks")).unwrap();
    let quoted_marker = shell_single_quote(&hook_marker);
    let repo_git_hook = stack_repo.join(".git/hooks/post-checkout");
    fs::write(
        &repo_git_hook,
        format!("#!/bin/sh\necho repo_hook >> {quoted_marker}\n"),
    )
    .unwrap();
    fs::set_permissions(&repo_git_hook, fs::Permissions::from_mode(0o755)).unwrap();

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !hook_marker.exists(),
        "planted .git/hooks/post-checkout must not run"
    );

    let wt_alpha = stack.join(WORKTREES_NAME).join("alpha");
    assert_eq!(git(&wt_alpha, &["rev-parse", "HEAD"]).trim(), trunk_sha);
    assert!(!wt_alpha.join("feature.txt").exists());

    let evil_wt_hooks = wt_alpha.join(EMPTY_HOOKS_NAME);
    fs::create_dir_all(&evil_wt_hooks).unwrap();
    let evil_hook = evil_wt_hooks.join("post-checkout");
    fs::write(
        &evil_hook,
        format!("#!/bin/sh\necho wt_hook >> {quoted_marker}\n"),
    )
    .unwrap();
    fs::set_permissions(&evil_hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(&wt_alpha, &["checkout", "-q", "HEAD"]);
    assert!(
        !hook_marker.exists(),
        "relative .switchyard-empty-hooks inside worktree must never execute"
    );
}

#[test]
fn init_from_with_explicit_branch_furnishes_direct_claims_and_queued_offers_from_base_branch() {
    let base = temp_dir("init-from-branch");
    let (src, _) = build_source_repo(&base);
    // Advance `trunk` with its own distinct commit so `trunk` and `feature-base` diverge,
    // then detach source HEAD at `trunk` so HEAD != `feature-base`.
    fs::write(src.join("trunk-only.txt"), "trunk divergence\n").unwrap();
    git(&src, &["add", "trunk-only.txt"]);
    git(&src, &["commit", "-q", "-m", "Divergent trunk commit"]);
    let trunk_sha = git(&src, &["rev-parse", "refs/heads/trunk"])
        .trim()
        .to_string();
    let feature_sha = git(&src, &["rev-parse", "refs/heads/feature-base"])
        .trim()
        .to_string();
    assert_ne!(trunk_sha, feature_sha);
    git(&src, &["checkout", "-q", "--detach", &trunk_sha]);

    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();

    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_str,
        "--from",
        &src_str,
        "--branch",
        "feature-base",
        "--range-count",
        "1",
    ]);
    assert_eq!(code, 0, "{err}");

    // Move `<stack>/repo` detached HEAD away from `feature-base` onto `trunk_sha` so any code
    // branching from `HEAD` instead of `refs/heads/<base_branch>` would fail this pin!
    git(
        &stack.join(REPO_NAME),
        &["checkout", "-q", "--detach", &trunk_sha],
    );

    // Direct claim branches from feature-base, not stack/repo HEAD.
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_alpha = stack.join(WORKTREES_NAME).join("alpha");
    assert_eq!(git(&wt_alpha, &["rev-parse", "HEAD"]).trim(), feature_sha);
    assert!(!wt_alpha.join("trunk-only.txt").exists());
    assert_eq!(
        fs::read_to_string(wt_alpha.join("feature.txt")).unwrap(),
        "Synthetic feature payload.\n"
    );

    // Queue beta, then release alpha so serve_offers furnishes beta from feature-base too.
    let (code, out, err) = run_cli(&["claim", "beta", "--stack", &stack_str, "--wait"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("waiting beta"), "{out}");

    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    let wt_beta = stack.join(WORKTREES_NAME).join("beta");
    assert!(wt_beta.is_dir());
    assert_eq!(git(&wt_beta, &["rev-parse", "HEAD"]).trim(), feature_sha);
    assert!(!wt_beta.join("trunk-only.txt").exists());
    assert_eq!(
        fs::read_to_string(wt_beta.join("feature.txt")).unwrap(),
        "Synthetic feature payload.\n"
    );
}

#[test]
fn init_branch_without_from_builds_synthetic_repo_on_named_branch_with_null_source() {
    let base = temp_dir("init-branch-only");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();

    let (code, _, err) = run_cli(&["init", "--stack", &stack_str, "--branch", "release/v2"]);
    assert_eq!(code, 0, "{err}");

    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(stack.join(CONFIG_NAME)).unwrap()).unwrap();
    assert_eq!(cfg["base_branch"], "release/v2");
    assert_eq!(cfg["source_repo"], serde_json::Value::Null);

    // HEAD in stack/repo must stand detached so refs/heads/<base_branch> is never checked out.
    assert_ne!(
        git_code(&stack.join(REPO_NAME), &["symbolic-ref", "--quiet", "HEAD"]),
        0
    );

    let base_sha = git(
        &stack.join(REPO_NAME),
        &["rev-parse", "refs/heads/release/v2"],
    )
    .trim()
    .to_string();
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        git(
            &stack.join(WORKTREES_NAME).join("alpha"),
            &["rev-parse", "HEAD"]
        )
        .trim(),
        base_sha
    );

    // Claiming an agent whose name equals base_branch (`trunk`) succeeds cleanly.
    let trunk_stack = base.join("trunk-stack");
    let trunk_stack_str = trunk_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &trunk_stack_str, "--branch", "trunk"]);
    assert_eq!(code, 0, "{err}");
    assert_ne!(
        git_code(
            &trunk_stack.join(REPO_NAME),
            &["symbolic-ref", "--quiet", "HEAD"]
        ),
        0
    );
    let trunk_sha = git(
        &trunk_stack.join(REPO_NAME),
        &["rev-parse", "refs/heads/trunk"],
    )
    .trim()
    .to_string();
    let (code, _, err) = run_cli(&["claim", "trunk", "--stack", &trunk_stack_str]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        git(
            &trunk_stack.join(WORKTREES_NAME).join("trunk"),
            &["rev-parse", "HEAD"]
        )
        .trim(),
        trunk_sha
    );
}

#[test]
fn init_from_rolls_back_mid_init_after_clone_and_handles_case_refs_and_unborn_head() {
    let base = temp_dir("init-rollback-and-refs");

    // 1. Mid-init rollback when source tracks reserved `.switchyard-empty-hooks` file
    //    (causing failure AFTER `git clone` created `<stack>/repo`).
    let bad_src = base.join("reserved-hooks-src");
    fs::create_dir_all(&bad_src).unwrap();
    git(&bad_src, &["init", "-q", "-b", "main"]);
    git(
        &bad_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&bad_src, &["config", "user.name", "synthetic"]);
    fs::write(bad_src.join(EMPTY_HOOKS_NAME), "occupied\n").unwrap();
    git(&bad_src, &["add", EMPTY_HOOKS_NAME]);
    git(&bad_src, &["commit", "-q", "-m", "Track reserved path"]);

    // 1a. Non-existent stack directory rolls back completely.
    let fresh_stack = base.join("fresh-rollback-stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &fresh_stack.to_string_lossy(),
        "--from",
        &bad_src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains(EMPTY_HOOKS_NAME), "{err}");
    assert!(!fresh_stack.exists());

    // 1b. Pre-existing stack directory keeps pre-existing files while rolling back
    //     repo, worktrees, switchyard.lock, switchyard-config.json, and switchyard-registry.json.
    let pre_stack = base.join("preexisting-rollback-stack");
    fs::create_dir_all(&pre_stack).unwrap();
    fs::write(pre_stack.join("keep.txt"), "survivor\n").unwrap();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &pre_stack.to_string_lossy(),
        "--from",
        &bad_src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(pre_stack.is_dir());
    assert_eq!(
        fs::read_to_string(pre_stack.join("keep.txt")).unwrap(),
        "survivor\n"
    );
    for created_name in [
        REPO_NAME,
        WORKTREES_NAME,
        LOCK_NAME,
        CONFIG_NAME,
        REGISTRY_NAME,
    ] {
        assert!(
            !pre_stack.join(created_name).exists(),
            "{created_name} was not rolled back"
        );
    }

    // 1c. Post-`save_config` rollback: pre-creating a directory at `switchyard-registry.json`
    //     causes `save_registry` to fail AFTER `save_config` has already written
    //     `switchyard-config.json`, proving `switchyard-config.json` is rolled back while
    //     pre-existing sibling files survive!
    let post_cfg_stack = base.join("post-save-config-rollback");
    fs::create_dir_all(post_cfg_stack.join(REGISTRY_NAME)).unwrap();
    fs::write(post_cfg_stack.join("sibling.txt"), "intact\n").unwrap();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &post_cfg_stack.to_string_lossy(),
        "--branch",
        "trunk",
    ]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(
        fs::read_to_string(post_cfg_stack.join("sibling.txt")).unwrap(),
        "intact\n"
    );
    assert!(post_cfg_stack.join(REGISTRY_NAME).is_dir());
    for created_name in [REPO_NAME, WORKTREES_NAME, LOCK_NAME, CONFIG_NAME] {
        assert!(
            !post_cfg_stack.join(created_name).exists(),
            "{created_name} was not rolled back after save_config"
        );
    }

    // 2. Source repo with mixed-case `Switchyard/old` branch, a `refs/remotes/origin/remote-only`
    //    branch, and `HEAD` pointing to an unborn orphan branch `wip`.
    let orphan_src = base.join("orphan-head-src");
    fs::create_dir_all(&orphan_src).unwrap();
    git(&orphan_src, &["init", "-q", "-b", "main"]);
    git(
        &orphan_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&orphan_src, &["config", "user.name", "synthetic"]);
    fs::write(orphan_src.join("README.md"), "main commit\n").unwrap();
    git(&orphan_src, &["add", "README.md"]);
    git(&orphan_src, &["commit", "-q", "-m", "Main commit"]);
    let main_sha = git(&orphan_src, &["rev-parse", "refs/heads/main"])
        .trim()
        .to_string();

    git(&orphan_src, &["branch", "Switchyard/old"]);
    git(
        &orphan_src,
        &["update-ref", "refs/remotes/origin/remote-only", &main_sha],
    );
    git(&orphan_src, &["checkout", "-q", "--orphan", "wip"]);
    git(&orphan_src, &["rm", "-rf", "-q", "--cached", "."]);

    // Omitting --branch when source HEAD is unborn refuses cleanly at exit 1.
    let no_branch_stack = base.join("unborn-head-no-branch");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &no_branch_stack.to_string_lossy(),
        "--from",
        &orphan_src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(!no_branch_stack.exists());

    // Explicit `--branch main` succeeds even though source HEAD sits on unborn `wip`,
    // and strips mixed-case `Switchyard/old` so `claim old` succeeds on APFS!
    let main_stack = base.join("unborn-head-main-stack");
    let main_stack_str = main_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &main_stack_str,
        "--from",
        &orphan_src.to_string_lossy(),
        "--branch",
        "main",
    ]);
    assert_eq!(code, 0, "{err}");
    let heads = git(
        &main_stack.join(REPO_NAME),
        &["for-each-ref", "--format=%(refname)", "refs/heads/"],
    );
    assert_eq!(heads.trim(), "refs/heads/main");
    let (code, _, err) = run_cli(&["claim", "old", "--stack", &main_stack_str]);
    assert_eq!(code, 0, "{err}");

    // Explicit `--branch remote-only` resolves from `refs/remotes/origin/remote-only` in source!
    let remote_stack = base.join("remote-origin-branch-stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &remote_stack.to_string_lossy(),
        "--from",
        &orphan_src.to_string_lossy(),
        "--branch",
        "remote-only",
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        git(
            &remote_stack.join(REPO_NAME),
            &["rev-parse", "refs/heads/remote-only"]
        )
        .trim(),
        main_sha
    );
    assert_eq!(
        git(
            &remote_stack.join(REPO_NAME),
            &["for-each-ref", "--format=%(refname)", "refs/remotes/"]
        )
        .trim(),
        ""
    );
}

#[test]
fn init_from_refuses_urls_missing_paths_unborn_detached_and_bad_branches_without_residue() {
    let base = temp_dir("init-refusals");
    let (src, _) = build_source_repo(&base);

    // 1. Remote and URL shapes refused at exit 1 before invoking git.
    for remote in [
        "https://example.invalid/repo.git",
        "http://example.invalid/repo.git",
        "ssh://example.invalid/repo.git",
        "git://example.invalid/repo.git",
        "file:///tmp/synthetic-repo",
        "git@example.invalid:org/repo.git",
        "host:org/repo.git",
    ] {
        let stack = base.join("url-stack");
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--from",
            remote,
        ]);
        assert_eq!(code, 1, "remote {remote} gave code {code}: {err}");
        assert!(err.contains("offline"), "remote {remote}: {err}");
        assert!(!stack.exists(), "residue left for {remote}");
    }

    // 2. Missing path, plain file, non-git dir, and subdirectory inside a git repo.
    let missing = base.join("does-not-exist");
    let plain_file = base.join("plain.txt");
    fs::write(&plain_file, "plain\n").unwrap();
    let plain_dir = base.join("plain-dir");
    fs::create_dir(&plain_dir).unwrap();
    let subdir = src.join("sub");
    fs::create_dir(&subdir).unwrap();

    for bad_src in [&missing, &plain_file, &plain_dir, &subdir] {
        let stack = base.join("bad-src-stack");
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--from",
            &bad_src.to_string_lossy(),
        ]);
        assert_eq!(code, 1, "{} gave {err}", bad_src.display());
        assert!(!stack.exists(), "residue for {}", bad_src.display());
    }

    // 3. Unborn / empty source repository (zero commits).
    let unborn = base.join("unborn");
    fs::create_dir(&unborn).unwrap();
    git(&unborn, &["init", "-q", "-b", "main"]);
    let stack = base.join("unborn-stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack.to_string_lossy(),
        "--from",
        &unborn.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("no commits"), "{err}");
    assert!(!stack.exists());

    // 4. Missing --branch in source repository (also test pre-created empty stack dir).
    let precreated = base.join("precreated-empty");
    fs::create_dir(&precreated).unwrap();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &precreated.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
        "--branch",
        "no-such-branch",
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("no-such-branch"), "{err}");
    assert_eq!(fs::read_dir(&precreated).unwrap().count(), 0);

    // 5. Detached HEAD in source repository without --branch.
    git(&src, &["checkout", "-q", "--detach"]);
    let detached_stack = base.join("detached-stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &detached_stack.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("detached HEAD"), "{err}");
    assert!(!detached_stack.exists());

    // 6. Valid 250-byte component and 255-byte multi-component branch names succeed,
    //    while 251-byte component and 256-byte total names are refused at exit 1.
    let valid_250 = "a".repeat(250);
    assert!(switchyard::is_valid_base_branch(&valid_250));
    let stack_250 = base.join("stack-250");
    let stack_250_str = stack_250.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_250_str, "--branch", &valid_250]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_250_str]);
    assert_eq!(code, 0, "{err}");

    let valid_255_multi = format!("{}/{}", "a".repeat(200), "b".repeat(54));
    assert_eq!(valid_255_multi.len(), 255);
    assert!(switchyard::is_valid_base_branch(&valid_255_multi));
    let stack_255 = base.join("stack-255-multi");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_255.to_string_lossy(),
        "--branch",
        &valid_255_multi,
    ]);
    assert_eq!(code, 0, "{err}");

    let comp_251 = "a".repeat(251);
    let overlong = format!("{}/{}", "a".repeat(200), "b".repeat(55));
    for bad_branch in [
        "",
        "-leading",
        ".leading",
        "trailing.",
        "feat/.hidden",
        "feat/end.",
        "feat/sub.LOCK/leaf",
        "two..dots",
        "trailing.lock",
        "main.LOCK",
        "trailing/",
        "/leading",
        "double//slash",
        "has space",
        "main@{0}",
        "bad\x07name",
        "bad\x7fname",
        "bad~1",
        "bad^",
        "bad:ref",
        "bad?",
        "bad*",
        "bad[0]",
        "bad\\ref",
        "HEAD",
        "head",
        "HeAd",
        "@",
        "switchyard",
        "switchyard/alpha",
        "Switchyard",
        "Switchyard/alpha",
        "SWITCHYARD/BETA",
        comp_251.as_str(),
        overlong.as_str(),
    ] {
        let bad_stack = base.join("bad-branch-stack");
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &bad_stack.to_string_lossy(),
            "--branch",
            bad_branch,
        ]);
        assert_eq!(code, 1, "branch {bad_branch:?} gave {code}: {err}");
        assert!(!bad_stack.exists(), "residue for {bad_branch:?}");
    }
}

#[test]
fn corrupt_switchyard_config_refuses_every_command_at_exit_2_without_mutating_disk() {
    let corrupt_configs = [
        "not valid json {".to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":2}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1,"extra":true}"#.to_string(),
        r#"{"base_branch":"main","probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","base_branch":"main","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"switchyard/alpha","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"Switchyard/alpha","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main.LOCK","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"../bad","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":"false","seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":true,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":{"PORT":"20000"},"probe_ports":false,"seed_files":[],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":false,"seed_files":["README.md"],"source_repo":null,"version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":"","version":1}"#.to_string(),
        r#"{"base_branch":"main","env_template":null,"probe_ports":false,"seed_files":[],"source_repo":"relative/path","version":1}"#.to_string(),
    ];

    for (idx, body) in corrupt_configs.iter().enumerate() {
        let base = temp_dir(&format!("corrupt-cfg-{idx}"));
        let stack = base.join("stack");
        let stack_str = stack.to_string_lossy().into_owned();
        let (code, _, err) = run_cli(&["init", "--stack", &stack_str, "--branch", "main"]);
        assert_eq!(code, 0, "{err}");

        fs::write(stack.join(CONFIG_NAME), body).unwrap();
        // Also test missing lock file on the first case to prove recreate_lock checks config!
        if idx == 0 {
            fs::remove_file(stack.join(LOCK_NAME)).unwrap();
        }
        let before = snapshot(&stack);

        for argv in [
            vec!["claim", "alpha", "--stack", &stack_str],
            vec!["list", "--stack", &stack_str],
            vec!["list", "--queue", "--stack", &stack_str],
            vec!["release", "alpha", "--stack", &stack_str],
            vec!["doctor", "--stack", &stack_str],
            vec!["doctor", "--fix", "--stack", &stack_str],
        ] {
            let (code, _, err) = run_cli(&argv);
            assert_eq!(code, 2, "case {idx} {argv:?}: {err}");
            assert!(err.contains("corrupt"), "case {idx} {argv:?}: {err}");
        }
        assert_eq!(snapshot(&stack), before, "case {idx} mutated stack");
    }
}

// =========================================================================
// Obligation SY-28: release --keep-branch, --safe, and --safe --keep-branch
// =========================================================================

#[test]
fn release_keep_branch_preserves_commits_serves_waits_and_blocks_duplicate_branch_reclaim() {
    let base = temp_dir("keep-branch");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str, "--range-count", "1"]);
    assert_eq!(code, 0, "{err}");

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_alpha = stack.join(WORKTREES_NAME).join("alpha");
    fs::write(wt_alpha.join("work.txt"), "important agent commit\n").unwrap();
    git(&wt_alpha, &["add", "work.txt"]);
    git(&wt_alpha, &["commit", "-q", "-m", "Agent alpha commit"]);
    let committed_sha = git(&wt_alpha, &["rev-parse", "HEAD"]).trim().to_string();

    // Queue beta behind alpha.
    let (code, _, err) = run_cli(&["claim", "beta", "--stack", &stack_str, "--wait"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run_cli(&["release", "alpha", "--keep-branch", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "released alpha\nbranch switchyard/alpha kept\nports 20000-20009 returned\n"
    );
    assert!(!wt_alpha.exists());
    // Branch switchyard/alpha and its commit survive in stack/repo!
    assert_eq!(
        git(
            &stack.join(REPO_NAME),
            &["rev-parse", "refs/heads/switchyard/alpha"]
        )
        .trim(),
        committed_sha
    );
    // Beta was furnished onto the freed slot.
    assert!(stack.join(WORKTREES_NAME).join("beta").is_dir());

    // Release beta so the pool slot is free again, then try re-claiming alpha while
    // refs/heads/switchyard/alpha still stands.
    let (code, _, err) = run_cli(&["release", "beta", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("switchyard/alpha"), "{err}");
    assert_eq!(
        git(
            &stack.join(REPO_NAME),
            &["rev-parse", "refs/heads/switchyard/alpha"]
        )
        .trim(),
        committed_sha
    );
}

#[test]
fn release_safe_succeeds_when_clean_and_refuses_dirty_or_ahead_without_mutating_state() {
    let base = temp_dir("release-safe");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    // 1. Clean untouched claim (holding untracked .env.switchyard) releases cleanly under --safe.
    let (code, _, err) = run_cli(&["claim", "clean", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert!(stack
        .join(WORKTREES_NAME)
        .join("clean")
        .join(ENV_NAME)
        .is_file());
    let (code, out, err) = run_cli(&["release", "clean", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("released clean"), "{out}");
    assert!(!stack.join(WORKTREES_NAME).join("clean").exists());

    // 2. Modified tracked file refuses at exit 1 and preserves claim, worktree, branch, ports.
    let (code, _, err) = run_cli(&["claim", "mod-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_mod = stack.join(WORKTREES_NAME).join("mod-agent");
    fs::write(wt_mod.join("README.md"), "modified tracked content\n").unwrap();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "mod-agent", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("uncommitted or untracked"), "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert_eq!(
        fs::read_to_string(wt_mod.join("README.md")).unwrap(),
        "modified tracked content\n"
    );

    // 3. Staged file refuses at exit 1.
    let (code, _, err) = run_cli(&["claim", "staged-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_staged = stack.join(WORKTREES_NAME).join("staged-agent");
    fs::write(wt_staged.join("staged.txt"), "staged work\n").unwrap();
    git(&wt_staged, &["add", "staged.txt"]);
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "staged-agent", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert!(wt_staged.join("staged.txt").is_file());

    // 4. Nested untracked file inside a subdirectory refuses at exit 1 (-uall pin!).
    let (code, _, err) = run_cli(&["claim", "untracked-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_untracked = stack.join(WORKTREES_NAME).join("untracked-agent");
    fs::create_dir_all(wt_untracked.join("nested/deep")).unwrap();
    fs::write(
        wt_untracked.join("nested/deep/notes.txt"),
        "untracked note\n",
    )
    .unwrap();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&[
        "release",
        "untracked-agent",
        "--safe",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert!(wt_untracked.join("nested/deep/notes.txt").is_file());

    // 5. Committed work ahead of base_branch refuses under --safe alone, then SUCCEEDS
    // under --safe --keep-branch while still refusing if dirty files also stand!
    let (code, _, err) = run_cli(&["claim", "ahead-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_ahead = stack.join(WORKTREES_NAME).join("ahead-agent");
    fs::write(wt_ahead.join("committed.txt"), "saved commit\n").unwrap();
    git(&wt_ahead, &["add", "committed.txt"]);
    git(&wt_ahead, &["commit", "-q", "-m", "Ahead commit"]);
    let ahead_sha = git(&wt_ahead, &["rev-parse", "HEAD"]).trim().to_string();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();

    // --safe without --keep-branch refuses because 1 unmerged commit stands ahead of main.
    let (code, _, err) = run_cli(&["release", "ahead-agent", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("1 unmerged commit ahead of main"), "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert!(wt_ahead.is_dir());

    // Plant an untracked file on top of the committed branch: --safe --keep-branch must refuse!
    fs::write(wt_ahead.join("dirty.txt"), "unsaved\n").unwrap();
    let (code, _, err) = run_cli(&[
        "release",
        "ahead-agent",
        "--safe",
        "--keep-branch",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(wt_ahead.join("dirty.txt").is_file());
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);

    // Clean the untracked file: --safe --keep-branch now succeeds and preserves ahead_sha!
    fs::remove_file(wt_ahead.join("dirty.txt")).unwrap();
    let (code, out, err) = run_cli(&[
        "release",
        "ahead-agent",
        "--safe",
        "--keep-branch",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("branch switchyard/ahead-agent kept"), "{out}");
    assert!(!wt_ahead.exists());
    assert_eq!(
        git(
            &stack.join(REPO_NAME),
            &["rev-parse", "refs/heads/switchyard/ahead-agent"]
        )
        .trim(),
        ahead_sha
    );
}

#[test]
fn release_with_safe_and_keep_branch_cancels_a_queued_wait_cleanly() {
    let base = temp_dir("release-wait-safe");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str, "--range-count", "1"]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "holder", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "waiter", "--stack", &stack_str, "--wait"]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run_cli(&[
        "release",
        "waiter",
        "--safe",
        "--keep-branch",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "released waiter\nwait cancelled\n");
}

#[test]
fn release_safe_covers_missing_worktree_tracked_env_deleted_renamed_non_default_base_detached_head_and_nested_git(
) {
    let base = temp_dir("release-safe-edges");

    // 1. Source repository tracking `.env.switchyard` on non-default branch `trunk`.
    let env_src = base.join("env-src");
    fs::create_dir_all(&env_src).unwrap();
    git(&env_src, &["init", "-q", "-b", "trunk"]);
    git(
        &env_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&env_src, &["config", "user.name", "synthetic"]);
    fs::write(env_src.join("README.md"), "trunk readme\n").unwrap();
    fs::write(env_src.join("extra.txt"), "extra tracked\n").unwrap();
    fs::write(env_src.join(ENV_NAME), "PORT=9999\n").unwrap();
    git(&env_src, &["add", "README.md", "extra.txt", ENV_NAME]);
    git(&env_src, &["commit", "-q", "-m", "Track env and readme"]);

    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_str,
        "--from",
        &env_src.to_string_lossy(),
        "--branch",
        "trunk",
    ]);
    assert_eq!(code, 0, "{err}");

    // 1a. Claim overwrites tracked `.env.switchyard` (` M` in porcelain): `release --safe` MUST allow it!
    let (code, _, err) = run_cli(&["claim", "env-ok", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_env = stack.join(WORKTREES_NAME).join("env-ok");
    let status_env = git(&wt_env, &["status", "--porcelain=v1"]);
    assert!(status_env.contains(" M .env.switchyard"), "{status_env}");
    let (code, _, err) = run_cli(&["release", "env-ok", "--safe", "--stack", &stack_str]);
    assert_eq!(
        code, 0,
        "tracked modified .env.switchyard must pass --safe: {err}"
    );

    // 2. Missing `<stack>/worktrees/<agent>` directory refuses at exit 1 without mutating claim/branch/ports.
    let (code, _, err) = run_cli(&["claim", "missing-wt", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    fs::remove_dir_all(stack.join(WORKTREES_NAME).join("missing-wt")).unwrap();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "missing-wt", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("missing or invalid"), "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert_eq!(
        git_code(
            &stack.join(REPO_NAME),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/switchyard/missing-wt"
            ]
        ),
        0
    );

    // 3. Deleted (`D`) and renamed (`R`) tracked files both refuse `release --safe` at exit 1.
    let (code, _, err) = run_cli(&["claim", "del-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_del = stack.join(WORKTREES_NAME).join("del-agent");
    fs::remove_file(wt_del.join("README.md")).unwrap();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "del-agent", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);

    let (code, _, err) = run_cli(&["claim", "ren-agent", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_ren = stack.join(WORKTREES_NAME).join("ren-agent");
    git(&wt_ren, &["mv", "README.md", "MOVED.md"]);
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let (code, _, err) = run_cli(&["release", "ren-agent", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
    assert!(wt_ren.join("MOVED.md").is_file());

    // 4. Non-default `base_branch` (`trunk`) ahead-commit refusal on `release --safe` and
    //    success on `release --safe --keep-branch`.
    let (code, _, err) = run_cli(&["claim", "trunk-ahead", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_trunk = stack.join(WORKTREES_NAME).join("trunk-ahead");
    git(
        &wt_trunk,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&wt_trunk, &["config", "user.name", "synthetic"]);
    fs::write(wt_trunk.join("ahead.txt"), "ahead on trunk\n").unwrap();
    git(&wt_trunk, &["add", "ahead.txt"]);
    git(&wt_trunk, &["commit", "-q", "-m", "Ahead of trunk"]);
    let trunk_ahead_sha = git(&wt_trunk, &["rev-parse", "HEAD"]).trim().to_string();
    // Leaving `.env.switchyard` as unstaged ` M` still passes the dirty check and hits the ahead check.
    let (code, _, err) = run_cli(&["release", "trunk-ahead", "--safe", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("1 unmerged commit ahead of trunk"), "{err}");
    let (code, _, err) = run_cli(&[
        "release",
        "trunk-ahead",
        "--safe",
        "--keep-branch",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        git(
            &stack.join(REPO_NAME),
            &["rev-parse", "refs/heads/switchyard/trunk-ahead"]
        )
        .trim(),
        trunk_ahead_sha
    );

    // 5. Detached-HEAD commits inside `worktrees/<agent>` refuse BOTH `release --safe` AND
    //    `release --safe --keep-branch` at exit 1, preserving the worktree and detached commits!
    let (code, _, err) = run_cli(&["claim", "detached-wt", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_det = stack.join(WORKTREES_NAME).join("detached-wt");
    git(
        &wt_det,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&wt_det, &["config", "user.name", "synthetic"]);
    git(&wt_det, &["checkout", "-q", "--detach"]);
    fs::write(wt_det.join("detached.txt"), "precious detached commit\n").unwrap();
    git(&wt_det, &["add", "detached.txt"]);
    git(&wt_det, &["commit", "-q", "-m", "Detached commit"]);
    let detached_sha = git(&wt_det, &["rev-parse", "HEAD"]).trim().to_string();
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();

    for argv in [
        vec!["release", "detached-wt", "--safe", "--stack", &stack_str],
        vec![
            "release",
            "detached-wt",
            "--safe",
            "--keep-branch",
            "--stack",
            &stack_str,
        ],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_eq!(code, 1, "{argv:?}: {err}");
        assert!(
            err.contains("not on branch switchyard/detached-wt"),
            "{err}"
        );
        assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
        assert_eq!(git(&wt_det, &["rev-parse", "HEAD"]).trim(), detached_sha);
    }

    // 6. Unborn nested `git init` directory (`mkdir sub && git -C sub init`) inside `worktrees/<agent>`
    //    is detected as unsaved changes even though `git status` skips unborn nested repos!
    let (code, _, err) = run_cli(&["claim", "nested-git", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_nested = stack.join(WORKTREES_NAME).join("nested-git");
    let nested_sub = wt_nested.join("sub");
    fs::create_dir_all(&nested_sub).unwrap();
    git(&nested_sub, &["init", "-q", "-b", "main"]);
    let reg_before = fs::read(stack.join(REGISTRY_NAME)).unwrap();

    for argv in [
        vec!["release", "nested-git", "--safe", "--stack", &stack_str],
        vec![
            "release",
            "nested-git",
            "--safe",
            "--keep-branch",
            "--stack",
            &stack_str,
        ],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_eq!(code, 1, "{argv:?}: {err}");
        assert!(err.contains("uncommitted or untracked"), "{err}");
        assert_eq!(fs::read(stack.join(REGISTRY_NAME)).unwrap(), reg_before);
        assert!(nested_sub.join(".git").is_dir());
    }
}

// =========================================================================
// Obligation SY-29: doctor and doctor --fix
// =========================================================================

#[test]
fn doctor_audits_and_doctor_fix_repairs_clean_residue_while_preserving_kept_branches() {
    let base = temp_dir("doctor-lifecycle");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();

    // Uninitialised stack exits 2 without creating switchyard.lock on an existing empty dir.
    let empty_uninit = base.join("empty-uninit");
    fs::create_dir_all(&empty_uninit).unwrap();
    let (code, _, err) = run_cli(&["doctor", "--stack", &empty_uninit.to_string_lossy()]);
    assert_eq!(code, 2, "{err}");
    assert!(!empty_uninit.join(LOCK_NAME).exists());
    assert_eq!(fs::read_dir(&empty_uninit).unwrap().count(), 0);

    let (code, _, err) = run_cli(&["doctor", "--stack", &stack_str]);
    assert_eq!(code, 2, "{err}");
    assert!(!stack.exists());

    let (code, _, err) = run_cli(&["init", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    // Read-only `doctor` when `switchyard.lock` was deleted succeeds without recreating it,
    // whereas `doctor --fix` recreates `switchyard.lock`.
    fs::remove_file(stack.join(LOCK_NAME)).unwrap();
    let (code, out, err) = run_cli(&["doctor", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "doctor ok\n");
    assert!(!stack.join(LOCK_NAME).exists());

    let (code, out, err) = run_cli(&["doctor", "--fix", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "doctor ok\n");
    assert!(stack.join(LOCK_NAME).is_file());

    // Create a legitimate kept_branch with 1 commit ahead via release --keep-branch.
    let (code, _, err) = run_cli(&["claim", "kept", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    let wt_kept = stack.join(WORKTREES_NAME).join("kept");
    fs::write(wt_kept.join("saved.txt"), "kept commit\n").unwrap();
    git(&wt_kept, &["add", "saved.txt"]);
    git(&wt_kept, &["commit", "-q", "-m", "Kept commit"]);
    let kept_sha = git(&wt_kept, &["rev-parse", "HEAD"]).trim().to_string();
    let (code, _, err) = run_cli(&[
        "release",
        "kept",
        "--safe",
        "--keep-branch",
        "--stack",
        &stack_str,
    ]);
    assert_eq!(code, 0, "{err}");

    // Doctor reports kept_branch and still exits 0!
    let (code, out, err) = run_cli(&["doctor", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("doctor ok"), "{out}");
    assert!(out.contains("kept_branch switchyard/kept"), "{out}");

    // Now stage fixable residue:
    // 1. Stale temp files (.tmp-stale and symlink .tmp-link and switchyard-registry.json.tmp-abcd).
    fs::write(stack.join(".tmp-stale"), "stale\n").unwrap();
    std::os::unix::fs::symlink(stack.join("nonexistent"), stack.join(".tmp-link")).unwrap();
    fs::write(
        stack.join(format!("{REGISTRY_NAME}.tmp-deadbeef")),
        "partial\n",
    )
    .unwrap();
    // 2. Orphan 0-ahead branch switchyard/ghost.
    let repo = stack.join(REPO_NAME);
    git(&repo, &["branch", "switchyard/ghost", "refs/heads/main"]);
    // 3. Simulated SIGKILL mid-claim: clean linked worktree worktrees/crashed on 0-ahead switchyard/crashed.
    let wt_crashed = stack.join(WORKTREES_NAME).join("crashed");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/crashed",
            &wt_crashed.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::write(wt_crashed.join(ENV_NAME), "PORT=20000\n").unwrap();
    // 4. Prunable worktree entry: create linked worktree worktrees/vanished then rm -rf its directory.
    let wt_vanished = stack.join(WORKTREES_NAME).join("vanished");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/vanished",
            &wt_vanished.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::remove_dir_all(&wt_vanished).unwrap();

    // Audit without --fix: exits 1, reports all issues, and mutates NOTHING on disk!
    let before_audit = snapshot(&stack);
    let (code, out, err) = run_cli(&["doctor", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}\n{out}");
    assert!(
        out.contains("issue unfixed prunable_worktree worktrees/vanished"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed orphan_worktree_dir worktrees/crashed"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed orphan_branch switchyard/ghost"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed stale_temp_file .tmp-stale"),
        "{out}"
    );
    assert!(out.contains("kept_branch switchyard/kept"), "{out}");
    assert_eq!(
        snapshot(&stack),
        before_audit,
        "doctor without --fix mutated disk"
    );

    // Now run doctor --fix: repairs every fixable issue in one pass, exits 0, and preserves switchyard/kept!
    let (code, out, err) = run_cli(&["doctor", "--fix", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}\n{out}");
    assert!(out.contains("doctor ok"), "{out}");
    assert!(
        out.contains("issue fixed prunable_worktree worktrees/vanished"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed orphan_worktree_dir worktrees/crashed"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed orphan_branch switchyard/ghost"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed stale_temp_file .tmp-stale"),
        "{out}"
    );
    assert!(out.contains("kept_branch switchyard/kept"), "{out}");
    assert!(!stack.join(".tmp-stale").exists());
    assert!(stack.join(".tmp-link").symlink_metadata().is_err());
    assert!(!stack.join(format!("{REGISTRY_NAME}.tmp-deadbeef")).exists());
    assert!(!wt_crashed.exists());
    assert_eq!(
        git(&repo, &["branch", "--list", "switchyard/ghost"]).trim(),
        ""
    );
    assert_eq!(
        git(&repo, &["branch", "--list", "switchyard/crashed"]).trim(),
        ""
    );
    assert_eq!(
        git(&repo, &["branch", "--list", "switchyard/vanished"]).trim(),
        ""
    );
    // Kept branch and its commit remain 100% untouched!
    assert_eq!(
        git(&repo, &["rev-parse", "refs/heads/switchyard/kept"]).trim(),
        kept_sha
    );

    // Agent `crashed` can now be claimed cleanly!
    let (code, _, err) = run_cli(&["claim", "crashed", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn doctor_reports_missing_active_worktree_and_dirty_orphan_worktree_without_deleting_dirty_files() {
    let base = temp_dir("doctor-unfixable");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    // Active claim alpha whose worktree directory is deleted outside switchyard.
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    fs::remove_dir_all(stack.join(WORKTREES_NAME).join("alpha")).unwrap();

    // Dirty orphan linked worktree holding uncommitted precious work.
    let repo = stack.join(REPO_NAME);
    let dirty_wt = stack.join(WORKTREES_NAME).join("dirty-orphan");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/dirty-orphan",
            &dirty_wt.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::write(dirty_wt.join("unsaved.rs"), "fn precious() {}\n").unwrap();

    // Plain non-worktree orphan directory holding an untracked file.
    let plain_dirty = stack.join(WORKTREES_NAME).join("plain-orphan");
    fs::create_dir_all(&plain_dirty).unwrap();
    fs::write(plain_dirty.join("notes.txt"), "keep me\n").unwrap();

    for argv in [
        vec!["doctor", "--stack", &stack_str],
        vec!["doctor", "--fix", "--stack", &stack_str],
    ] {
        let (code, out, err) = run_cli(&argv);
        assert_eq!(code, 1, "{argv:?}: {err}\n{out}");
        assert!(
            out.contains("issue unfixed missing_worktree worktrees/alpha"),
            "{out}"
        );
        assert!(
            out.contains("issue unfixed dirty_orphan_worktree worktrees/dirty-orphan"),
            "{out}"
        );
        assert!(
            out.contains("issue unfixed dirty_orphan_worktree worktrees/plain-orphan"),
            "{out}"
        );
    }
    // Neither dirty directory lost its files under doctor --fix!
    assert_eq!(
        fs::read_to_string(dirty_wt.join("unsaved.rs")).unwrap(),
        "fn precious() {}\n"
    );
    assert_eq!(
        fs::read_to_string(plain_dirty.join("notes.txt")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn doctor_checks_stdout_writes_against_a_closed_pipe() {
    let base = temp_dir("doctor-pipe");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");

    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args(["doctor", "--stack", &stack_str])
        .stdout(Stdio::from(writer))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the command's output could not be written"),
        "{stderr}"
    );
}

#[test]
fn doctor_covers_empty_dir_detached_orphan_symlink_git_nested_unborn_temp_predicate_corrupt_registry_and_exact_order(
) {
    let base = temp_dir("doctor-edges-and-order");
    let stack = base.join("stack");
    let stack_str = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_str, "--branch", "main"]);
    assert_eq!(code, 0, "{err}");
    let repo = stack.join(REPO_NAME);

    // Stage all 5 issue classes simultaneously so we also pin exact deterministic ordering!
    // Class 1 (`missing_worktree`): active claim `active-missing` whose worktree directory was deleted.
    let (code, _, err) = run_cli(&["claim", "active-missing", "--stack", &stack_str]);
    assert_eq!(code, 0, "{err}");
    fs::remove_dir_all(stack.join(WORKTREES_NAME).join("active-missing")).unwrap();

    // Class 2 (`prunable_worktree`): unclaimed worktree `prunable-wt` deleted from disk.
    let wt_prunable = stack.join(WORKTREES_NAME).join("prunable-wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/prunable-wt",
            &wt_prunable.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::remove_dir_all(&wt_prunable).unwrap();

    // Class 3a (`dirty_orphan_worktree`): unclaimed linked worktree on detached HEAD with 1 commit ahead of main.
    let wt_det_orphan = stack.join(WORKTREES_NAME).join("aa-detached-orphan");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            &wt_det_orphan.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    git(
        &wt_det_orphan,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&wt_det_orphan, &["config", "user.name", "synthetic"]);
    fs::write(wt_det_orphan.join("det.txt"), "detached orphan work\n").unwrap();
    git(&wt_det_orphan, &["add", "det.txt"]);
    git(
        &wt_det_orphan,
        &["commit", "-q", "-m", "Detached orphan commit"],
    );
    let det_orphan_sha = git(&wt_det_orphan, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Class 3b (`orphan_worktree_dir`): unclaimed empty directory `bb-empty-orphan` with no `.git`.
    let wt_empty = stack.join(WORKTREES_NAME).join("bb-empty-orphan");
    fs::create_dir_all(&wt_empty).unwrap();

    // Class 3c (`dirty_orphan_worktree`): unclaimed linked worktree with an unborn nested `git init` dir.
    let wt_nested_orphan = stack.join(WORKTREES_NAME).join("cc-nested-unborn");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/cc-nested-unborn",
            &wt_nested_orphan.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    let nested_sub = wt_nested_orphan.join("sub");
    fs::create_dir_all(&nested_sub).unwrap();
    git(&nested_sub, &["init", "-q", "-b", "main"]);

    // Class 3d (`dirty_orphan_worktree`): spoofed directory with a symlinked `.git` entry.
    let wt_spoof = stack.join(WORKTREES_NAME).join("dd-spoof-git");
    fs::create_dir_all(&wt_spoof).unwrap();
    std::os::unix::fs::symlink(repo.join(".git"), wt_spoof.join(".git")).unwrap();

    // Class 4 (`orphan_branch`): 0-ahead unclaimed branch `switchyard/zz-orphan-branch`.
    git(
        &repo,
        &["branch", "switchyard/zz-orphan-branch", "refs/heads/main"],
    );

    // Class 5 (`stale_temp_file`): `switchyard-config.json.tmp-stale` (file, fixable) and
    // `.tmp-dir` (directory, reported as stale_temp_file but NOT unlinked by --fix), while
    // `notes.tmp-backup.txt` is NOT a switchyard temp file and must be completely ignored!
    let cfg_tmp = stack.join(format!("{CONFIG_NAME}.tmp-stale"));
    fs::write(&cfg_tmp, "partial config\n").unwrap();
    let tmp_dir_entry = stack.join(".tmp-dir");
    fs::create_dir_all(&tmp_dir_entry).unwrap();
    let user_notes = stack.join("notes.tmp-backup.txt");
    fs::write(&user_notes, "keep user file\n").unwrap();

    // 1. Audit without `--fix`: verify exact ordered sequence of issue kinds and targets!
    let (code, out, err) = run_cli(&["doctor", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}\n{out}");
    assert!(
        !out.contains("notes.tmp-backup.txt"),
        "unrelated file must not be reported: {out}"
    );
    let issue_lines: Vec<(&str, &str, &str)> = out
        .lines()
        .filter_map(|line| line.strip_prefix("issue "))
        .map(|rest| {
            let mut parts = rest.splitn(4, ' ');
            (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        issue_lines,
        vec![
            ("unfixed", "missing_worktree", "worktrees/active-missing"),
            ("unfixed", "prunable_worktree", "worktrees/prunable-wt"),
            (
                "unfixed",
                "dirty_orphan_worktree",
                "worktrees/aa-detached-orphan"
            ),
            (
                "unfixed",
                "orphan_worktree_dir",
                "worktrees/bb-empty-orphan"
            ),
            (
                "unfixed",
                "dirty_orphan_worktree",
                "worktrees/cc-nested-unborn"
            ),
            ("unfixed", "dirty_orphan_worktree", "worktrees/dd-spoof-git"),
            ("unfixed", "orphan_branch", "switchyard/cc-nested-unborn"),
            ("unfixed", "orphan_branch", "switchyard/prunable-wt"),
            ("unfixed", "orphan_branch", "switchyard/zz-orphan-branch"),
            ("unfixed", "stale_temp_file", ".tmp-dir"),
            (
                "unfixed",
                "stale_temp_file",
                "switchyard-config.json.tmp-stale"
            ),
        ]
    );

    // 2. Run `doctor --fix`: fixes prunable_worktree, empty orphan dir, 0-ahead orphan branches,
    //    and regular stale temp file, while preserving active-missing's `.git/worktrees` entry,
    //    detached-HEAD orphan commits, unborn nested git dir, spoofed .git symlink, `.tmp-dir`,
    //    and `notes.tmp-backup.txt`!
    assert!(repo.join(".git/worktrees/active-missing").is_dir());
    assert!(repo.join(".git/worktrees/prunable-wt").is_dir());

    let (code, out, err) = run_cli(&["doctor", "--fix", "--stack", &stack_str]);
    assert_eq!(code, 1, "{err}\n{out}");
    assert!(
        out.contains("issue unfixed missing_worktree worktrees/active-missing"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed prunable_worktree worktrees/prunable-wt"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed orphan_worktree_dir worktrees/bb-empty-orphan"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed orphan_branch switchyard/cc-nested-unborn"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed orphan_branch switchyard/prunable-wt"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed orphan_branch switchyard/zz-orphan-branch"),
        "{out}"
    );
    assert!(
        out.contains("issue fixed stale_temp_file switchyard-config.json.tmp-stale"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed stale_temp_file .tmp-dir"),
        "{out}"
    );
    // Targeted prunable cleanup removed `prunable-wt`'s admin dir while keeping `active-missing`'s!
    assert!(!repo.join(".git/worktrees/prunable-wt").exists());
    assert!(repo.join(".git/worktrees/active-missing").is_dir());
    assert!(!wt_empty.exists());
    assert!(!cfg_tmp.exists());
    assert!(tmp_dir_entry.is_dir());
    assert_eq!(fs::read_to_string(&user_notes).unwrap(), "keep user file\n");
    assert_eq!(
        git(&wt_det_orphan, &["rev-parse", "HEAD"]).trim(),
        det_orphan_sha
    );
    assert!(nested_sub.join(".git").is_dir());
    assert!(wt_spoof
        .join(".git")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());

    // 3. Corrupt `switchyard-registry.json` refuses `doctor` and `doctor --fix` at exit 2 without mutating disk!
    fs::write(stack.join(REGISTRY_NAME), "{corrupt registry\n").unwrap();
    let snap_corrupt = snapshot(&stack);
    for argv in [
        vec!["doctor", "--stack", &stack_str],
        vec!["doctor", "--fix", "--stack", &stack_str],
    ] {
        let (code, _, err) = run_cli(&argv);
        assert_eq!(code, 2, "{argv:?}: {err}");
        assert!(err.contains("corrupt"), "{err}");
    }
    assert_eq!(snapshot(&stack), snap_corrupt);
}

#[test]
fn symlink_env_smudge_filters_ignored_orphans_judge_arg_safe_corrupt_config_routing_and_missing_kept_branch(
) {
    let base = temp_dir("coderabbit-pins");

    // (a) Tracked symlink `.env.switchyard` in source repo is refused without overwriting target!
    let outside_secret = base.join("outside-secret.txt");
    fs::write(&outside_secret, "DO_NOT_OVERWRITE\n").unwrap();
    let sym_src = base.join("sym-env-src");
    fs::create_dir_all(&sym_src).unwrap();
    git(&sym_src, &["init", "-q", "-b", "main"]);
    git(
        &sym_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&sym_src, &["config", "user.name", "synthetic"]);
    std::os::unix::fs::symlink(&outside_secret, sym_src.join(ENV_NAME)).unwrap();
    git(&sym_src, &["add", ENV_NAME]);
    git(&sym_src, &["commit", "-q", "-m", "Track symlink env"]);

    let sym_stack = base.join("sym-stack");
    let sym_stack_str = sym_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &sym_stack_str,
        "--from",
        &sym_src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "sym-agent", "--stack", &sym_stack_str]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("non-regular entry"), "{err}");
    assert_eq!(
        fs::read_to_string(&outside_secret).unwrap(),
        "DO_NOT_OVERWRITE\n"
    );
    assert!(!sym_stack.join(WORKTREES_NAME).join("sym-agent").exists());
    assert_eq!(
        git(
            &sym_stack.join(REPO_NAME),
            &["branch", "--list", "switchyard/sym-agent"]
        )
        .trim(),
        ""
    );

    // (b) Inherited global `filter.lfs.smudge`/`process`/`required=true` neutralized on `init --from` & `claim`!
    let fake_home = base.join("fake-home");
    fs::create_dir_all(&fake_home).unwrap();
    let filter_marker = base.join("filter-ran.marker");
    let quoted_filter_marker = shell_single_quote(&filter_marker);
    fs::write(
        fake_home.join(".gitconfig"),
        format!(
            "[filter \"lfs\"]\n\tsmudge = /bin/sh -c 'echo smudge >> {quoted_filter_marker}; exit 1'\n\tprocess = /bin/sh -c 'echo process >> {quoted_filter_marker}; exit 1'\n\trequired = true\n"
        ),
    )
    .unwrap();

    let lfs_src = base.join("lfs-src");
    fs::create_dir_all(&lfs_src).unwrap();
    git(&lfs_src, &["init", "-q", "-b", "main"]);
    git(
        &lfs_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&lfs_src, &["config", "user.name", "synthetic"]);
    fs::write(lfs_src.join(".gitattributes"), "*.bin filter=lfs -text\n").unwrap();
    let pointer_body = "version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 0\n";
    fs::write(lfs_src.join("asset.bin"), pointer_body).unwrap();
    git(
        &lfs_src,
        &[
            "-c",
            "filter.lfs.clean=cat",
            "-c",
            "filter.lfs.smudge=cat",
            "-c",
            "filter.lfs.process=",
            "-c",
            "filter.lfs.required=false",
            "add",
            ".gitattributes",
            "asset.bin",
        ],
    );
    git(&lfs_src, &["commit", "-q", "-m", "Add LFS pointer"]);

    let lfs_stack = base.join("lfs-stack");
    let lfs_out = Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args([
            "init",
            "--stack",
            &lfs_stack.to_string_lossy(),
            "--from",
            &lfs_src.to_string_lossy(),
        ])
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", &fake_home)
        .output()
        .unwrap();
    assert_eq!(
        lfs_out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&lfs_out.stderr)
    );
    assert!(
        !filter_marker.exists(),
        "filter executed during init --from"
    );

    let lfs_claim = Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args([
            "claim",
            "lfs-agent",
            "--stack",
            &lfs_stack.to_string_lossy(),
        ])
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", &fake_home)
        .output()
        .unwrap();
    assert_eq!(
        lfs_claim.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&lfs_claim.stderr)
    );
    assert!(!filter_marker.exists(), "filter executed during claim");
    assert_eq!(
        fs::read_to_string(lfs_stack.join(WORKTREES_NAME).join("lfs-agent/asset.bin")).unwrap(),
        pointer_body
    );

    // (c) Gitignored local file (`notes.ignored`) inside an orphan worktree is preserved as
    //     `dirty_orphan_worktree` by `doctor --fix`, even when `.env.switchyard` is also gitignored,
    //     whereas `release --safe` on an active claim ignores gitignored build outputs!
    let ig_src = base.join("ig-src");
    fs::create_dir_all(&ig_src).unwrap();
    git(&ig_src, &["init", "-q", "-b", "main"]);
    git(
        &ig_src,
        &["config", "user.email", "synthetic@example.invalid"],
    );
    git(&ig_src, &["config", "user.name", "synthetic"]);
    fs::write(ig_src.join(".gitignore"), ".env.switchyard\n*.ignored\n").unwrap();
    git(&ig_src, &["add", ".gitignore"]);
    git(&ig_src, &["commit", "-q", "-m", "Add gitignore"]);

    let ig_stack = base.join("ig-stack");
    let ig_stack_str = ig_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &ig_stack_str,
        "--from",
        &ig_src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");

    // Active claim with `notes.ignored` still passes `release --safe`!
    let (code, _, err) = run_cli(&["claim", "active-ig", "--stack", &ig_stack_str]);
    assert_eq!(code, 0, "{err}");
    fs::write(
        ig_stack
            .join(WORKTREES_NAME)
            .join("active-ig/notes.ignored"),
        "build cache\n",
    )
    .unwrap();
    let (code, _, err) = run_cli(&["release", "active-ig", "--safe", "--stack", &ig_stack_str]);
    assert_eq!(code, 0, "{err}");

    let ig_repo = ig_stack.join(REPO_NAME);
    // Clean orphan worktree holding ONLY gitignored `.env.switchyard` -> fixed by `doctor --fix`!
    let wt_env_only = ig_stack.join(WORKTREES_NAME).join("clean-ig-env");
    git(
        &ig_repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/clean-ig-env",
            &wt_env_only.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::write(wt_env_only.join(ENV_NAME), "PORT=20000\n").unwrap();

    // Orphan worktree holding gitignored `notes.ignored` -> `dirty_orphan_worktree`, preserved!
    let wt_user_ig = ig_stack.join(WORKTREES_NAME).join("user-ig-orphan");
    git(
        &ig_repo,
        &[
            "worktree",
            "add",
            "-b",
            "switchyard/user-ig-orphan",
            &wt_user_ig.to_string_lossy(),
            "refs/heads/main",
        ],
    );
    fs::write(wt_user_ig.join(ENV_NAME), "PORT=20010\n").unwrap();
    fs::write(
        wt_user_ig.join("notes.ignored"),
        "precious ignored local db\n",
    )
    .unwrap();

    let (code, out, err) = run_cli(&["doctor", "--fix", "--stack", &ig_stack_str]);
    assert_eq!(code, 1, "{err}\n{out}");
    assert!(
        out.contains("issue fixed orphan_worktree_dir worktrees/clean-ig-env"),
        "{out}"
    );
    assert!(
        out.contains("issue unfixed dirty_orphan_worktree worktrees/user-ig-orphan"),
        "{out}"
    );
    assert!(!wt_env_only.exists());
    assert_eq!(
        fs::read_to_string(wt_user_ig.join("notes.ignored")).unwrap(),
        "precious ignored local db\n"
    );

    // (f) `claim --judge-arg --safe` forwards `--safe` to the judge, AND post-consultation
    //     corrupt `switchyard-config.json` exits 2 without writing `switchyard-routing.jsonl`!
    let judge_stack = base.join("judge-stack");
    let judge_stack_str = judge_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &judge_stack_str, "--branch", "main"]);
    assert_eq!(code, 0, "{err}");

    let captured_args = base.join("judge-args.txt");
    let quoted_args = shell_single_quote(&captured_args);
    let judge_staged = base.join("judge-ok.staged");
    fs::write(
        &judge_staged,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$@\" > {quoted_args}\nprintf '%s' '{{\"model\":\"jev-1.13.0\",\"answers\":{{\"profile\":{{\"type\":\"choice\",\"choice\":\"standard\",\"probabilities\":{{\"lean\":0.05,\"standard\":0.9,\"roomy\":0.03,\"other\":0.02}}}}}}}}'\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&judge_staged, fs::Permissions::from_mode(0o755)).unwrap();
    let judge_ok = base.join("judge-ok.sh");
    fs::rename(&judge_staged, &judge_ok).unwrap();

    let (code, _, err) = run_cli(&[
        "claim",
        "judged",
        "--stack",
        &judge_stack_str,
        "--describe",
        "Run three services side by side",
        "--default-profile",
        "lean",
        "--judge-command",
        &judge_ok.to_string_lossy(),
        "--judge-arg",
        "--safe",
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(fs::read_to_string(&captured_args).unwrap(), "--safe\n");

    // Fresh stack for post-consultation corrupt-config check so routing log starts absent!
    let corrupt_post_stack = base.join("corrupt-post-stack");
    let corrupt_post_str = corrupt_post_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &corrupt_post_str, "--branch", "main"]);
    assert_eq!(code, 0, "{err}");
    let quoted_cfg = shell_single_quote(&corrupt_post_stack.join(CONFIG_NAME));
    let corrupt_judge_staged = base.join("judge-corrupt.staged");
    fs::write(
        &corrupt_judge_staged,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '{{bad' > {quoted_cfg}\nprintf '%s' '{{\"model\":\"jev-1.13.0\",\"answers\":{{\"profile\":{{\"type\":\"choice\",\"choice\":\"lean\",\"probabilities\":{{\"lean\":0.9,\"standard\":0.05,\"roomy\":0.03,\"other\":0.02}}}}}}}}'\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&corrupt_judge_staged, fs::Permissions::from_mode(0o755)).unwrap();
    let corrupt_judge = base.join("judge-corrupt.sh");
    fs::rename(&corrupt_judge_staged, &corrupt_judge).unwrap();

    let (code, _, err) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        &corrupt_post_str,
        "--describe",
        "Run one service",
        "--default-profile",
        "lean",
        "--judge-command",
        &corrupt_judge.to_string_lossy(),
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("corrupt"), "{err}");
    assert!(
        !corrupt_post_stack.join(ROUTING_LOG_NAME).exists(),
        "corrupt switchyard-config.json must not append to switchyard-routing.jsonl"
    );

    // Truthful `branch ... kept`: if `refs/heads/switchyard/<agent>` was externally deleted
    // before `release --keep-branch` (without `--safe`), CLI does not claim the branch was kept!
    let wt_judged = judge_stack.join(WORKTREES_NAME).join("judged");
    git(&wt_judged, &["checkout", "-q", "--detach"]);
    git(
        &judge_stack.join(REPO_NAME),
        &["branch", "-D", "switchyard/judged"],
    );
    let (code, out, err) = run_cli(&[
        "release",
        "judged",
        "--keep-branch",
        "--stack",
        &judge_stack_str,
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("kept"), "{out}");
    assert_eq!(out, "released judged\nports 20000-20019 returned\n");
}
