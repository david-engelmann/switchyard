//! Discriminating integration tests for slice SY-S3 obligations
//! SY-30 (IPv4 and IPv6 host port collision probing across init, claim,
//! waiting claims, and release offers) and SY-31 (enriched
//! `.env.switchyard`, `SWITCHYARD_`-protected `--env-template`, and
//! SHA-256-tracked symlink-safe `.worktreeinclude` seeding).
//!
//! Every fixture is synthetic and runs completely offline inside
//! temporary directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, TcpListener};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use switchyard::{
    claim_profile_with_prober, claim_routed_waiting_with_options, claim_routed_with_options,
    claim_waiting_with_options, claim_with_prober, init_stack, init_stack_with_options,
    load_registry, release_report_with_prober, resolve_path, sha256_hex, ClaimOptions,
    ClaimOutcome, InitOptions, JudgeTransport, LoopbackProber, PortProber, Profile, ReleaseOptions,
    CONFIG_NAME, ENV_NAME, LOCK_NAME, QUEUE_LOG_NAME, REGISTRY_NAME, REPO_NAME, ROUTING_LOG_NAME,
    SEED_DIR_NAME, WORKTREEINCLUDE_NAME, WORKTREES_NAME,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static PORT_CURSOR: AtomicU16 = AtomicU16::new(31_000);

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
        "switchyard-s3-test-{}-{n}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
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

fn init_source_repo(root: &Path) -> PathBuf {
    let src = root.join("src_repo");
    fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["config", "user.email", "s3@example.invalid"]);
    git(&src, &["config", "user.name", "s3"]);
    fs::write(src.join("README.md"), "synthetic source repo\n").unwrap();
    git(&src, &["add", "README.md"]);
    git(&src, &["commit", "-q", "-m", "initial commit"]);
    src
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
    if root.exists() {
        walk(root, root, &mut out);
    }
    out
}

/// Find a contiguous block of `total_ports` ports currently free on both
/// IPv4 and IPv6 loopback, stepping an atomic cursor so parallel tests
/// probe disjoint port regions.
fn find_free_port_block(total_ports: u16) -> u16 {
    let prober = LoopbackProber;
    for _ in 0..500 {
        let base = PORT_CURSOR.fetch_add(total_ports.max(16) + 8, Ordering::SeqCst);
        let candidate = if !(25_000..=60_000).contains(&base) {
            PORT_CURSOR.store(31_000, Ordering::SeqCst);
            31_000
        } else {
            base
        };
        if (0..total_ports).all(|offset| prober.is_port_free(candidate + offset)) {
            return candidate;
        }
    }
    panic!("could not find {total_ports} contiguous free loopback ports");
}

/// Bind an IPv4 loopback listener at `offset_in_pool` inside a freshly
/// located contiguous pool of `total_ports` ports, retrying if another
/// process raced for any port in the block.
fn bind_ipv4_in_pool(total_ports: u16, offset_in_pool: u16) -> (u16, TcpListener) {
    let prober = LoopbackProber;
    for _ in 0..200 {
        let start = find_free_port_block(total_ports);
        let target = start + offset_in_pool;
        let Ok(listener) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, target)) else {
            continue;
        };
        let others_free = (0..total_ports)
            .filter(|&off| off != offset_in_pool)
            .all(|off| prober.is_port_free(start + off));
        if others_free {
            return (start, listener);
        }
    }
    panic!("could not bind IPv4 listener inside a clean port block");
}

/// Bind an IPv6 loopback listener at `offset_in_pool` inside a freshly
/// located contiguous pool of `total_ports` ports, returning `None` only
/// on hosts where IPv6 loopback itself is unavailable.
fn bind_ipv6_in_pool(total_ports: u16, offset_in_pool: u16) -> Option<(u16, TcpListener)> {
    match TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0)) {
        Ok(_) => {}
        Err(err)
            if err.kind() == std::io::ErrorKind::AddrNotAvailable
                || err.raw_os_error() == Some(libc::EAFNOSUPPORT)
                || err.raw_os_error() == Some(libc::EPROTONOSUPPORT) =>
        {
            return None;
        }
        Err(err) => panic!("unexpected IPv6 loopback probe failure: {err}"),
    }
    let prober = LoopbackProber;
    for _ in 0..200 {
        let start = find_free_port_block(total_ports);
        let target = start + offset_in_pool;
        let Ok(listener) = TcpListener::bind(SocketAddrV6::new(Ipv6Addr::LOCALHOST, target, 0, 0))
        else {
            continue;
        };
        let others_free = (0..total_ports)
            .filter(|&off| off != offset_in_pool)
            .all(|off| prober.is_port_free(start + off));
        if others_free {
            return Some((start, listener));
        }
    }
    panic!("could not bind IPv6 listener inside a clean port block");
}

fn verdict_body(choice: &str, confidence: f64) -> String {
    let rest = (1.0 - confidence) / 3.0;
    let mut map = serde_json::Map::new();
    for label in ["lean", "standard", "roomy", "other"] {
        map.insert(
            label.to_string(),
            serde_json::json!(if label == choice { confidence } else { rest }),
        );
    }
    serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            "profile": {
                "type": "choice",
                "choice": choice,
                "confidence": 0.5,
                "probabilities": map,
            }
        },
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

fn score_body(choice: &str, confidence: f64) -> String {
    let labels = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "other"];
    let rest = (1.0 - confidence) / 10.0;
    let mut map = serde_json::Map::new();
    for label in labels {
        map.insert(
            label.to_string(),
            serde_json::json!(if label == choice { confidence } else { rest }),
        );
    }
    serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            "score": {
                "type": "choice",
                "choice": choice,
                "confidence": 0.5,
                "probabilities": map,
            }
        },
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

struct StubJudge {
    verdict: &'static str,
    score: &'static str,
    calls: std::cell::RefCell<Vec<String>>,
}

impl StubJudge {
    fn new(verdict: &'static str, score: &'static str) -> Self {
        Self {
            verdict,
            score,
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl JudgeTransport for StubJudge {
    fn consult(&self, request: &str) -> Result<String, String> {
        self.calls.borrow_mut().push(request.to_string());
        if request.contains("\"profile\"") {
            Ok(verdict_body(self.verdict, 0.9))
        } else {
            Ok(score_body(self.score, 0.8))
        }
    }
}

struct BusySetProber {
    busy: BTreeSet<u16>,
}

impl PortProber for BusySetProber {
    fn is_port_free(&self, port: u16) -> bool {
        !self.busy.contains(&port)
    }
}

// =====================================================================
// SY-30: IPv4 + IPv6 Host Port Collision Probing
// =====================================================================

#[test]
fn ipv4_bound_tail_port_is_taken_without_probe_and_skipped_with_probe() {
    let tmp = temp_dir("ipv4-probe");
    // Bind the LAST port of range 0 (offset 3 in a 4-port range) so an
    // implementation that probed only `port_start` would fail this pin.
    let (base, listener) = bind_ipv4_in_pool(12, 3);
    let base_s = base.to_string();

    let unprobed = tmp.join("unprobed");
    let unprobed_s = unprobed.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &unprobed_s,
        "--port-start",
        &base_s,
        "--range-size",
        "4",
        "--range-count",
        "3",
    ]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run_cli(&["claim", "alpha", "--stack", &unprobed_s]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("ports {base}-{}", base + 3)),
        "without --probe-ports, claim allocates range 0: {out}"
    );

    let probed = tmp.join("probed");
    let probed_s = probed.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &probed_s,
        "--port-start",
        &base_s,
        "--range-size",
        "4",
        "--range-count",
        "3",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !probed.join(CONFIG_NAME).exists(),
        "bare init without flags writes no switchyard-config.json"
    );

    let (code, out, err) = run_cli(&["claim", "alpha", "--stack", &probed_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("ports {}-{}", base + 4, base + 7)),
        "with --probe-ports, claim skips range 0 and allocates range 1: {out}"
    );

    // Drop the IPv4 listener; range 0 is now unbound and claimable again.
    drop(listener);
    let (code, out, err) = run_cli(&["claim", "beta", "--stack", &probed_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("ports {base}-{}", base + 3)),
        "once listener closes, range 0 passes loopback probe: {out}"
    );
}

#[test]
fn ipv6_only_listener_is_detected_and_multi_range_profiles_skip_fragmented_spans() {
    let Some((base, v6_listener)) = bind_ipv6_in_pool(16, 5) else {
        return;
    };
    let bound_port = base + 5;
    // Prove on this host that an IPv4-only bind on `bound_port` succeeds,
    // so an IPv4-only prober would miss `v6_listener`.
    let v4_probe = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, bound_port));
    assert!(
        v4_probe.is_ok(),
        "127.0.0.1:{bound_port} should remain bindable while [::1]:{bound_port} is held"
    );
    drop(v4_probe);

    let tmp = temp_dir("ipv6-probe");
    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let base_s = base.to_string();
    // 4 ranges of 4 ports:
    // range 0 = base..base+3 (free)
    // range 1 = base+4..base+7 (port base+5 held on [::1]!)
    // range 2 = base+8..base+11 (free)
    // range 3 = base+12..base+15 (free)
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--port-start",
        &base_s,
        "--range-size",
        "4",
        "--range-count",
        "4",
    ]);
    assert_eq!(code, 0, "{err}");

    // Standard profile needs 2 contiguous unbound ranges: (0, 1) fails
    // because range 1 has [::1]:(base+5) bound, so it must skip to (2, 3)!
    let (code, out, err) = run_cli(&[
        "claim",
        "wide",
        "--stack",
        &stack_s,
        "--profile",
        "standard",
        "--probe-ports",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("ports {}-{}", base + 8, base + 15)),
        "standard profile should skip [::1]-occupied range 1 and land on ranges 2..=3: {out}"
    );

    // Now claim `alpha` (1 range) with --probe-ports: range 0 is free, so it takes range 0,
    // and claiming `beta` (1 range) with --probe-ports must refuse because only range 1
    // remains in the registry and [::1]:(base+5) holds it!
    let (code, out, err) = run_cli(&["claim", "alpha", "--stack", &stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains(&format!("ports {base}-{}", base + 3)));

    let (code, _, err) = run_cli(&["claim", "beta", "--stack", &stack_s, "--probe-ports"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("host loopback ports are occupied"),
        "expected host port occupation refusal, got: {err}"
    );
    drop(v6_listener);
}

#[test]
fn init_probe_ports_persists_in_config_and_governs_claims_and_release_offers() {
    let tmp = temp_dir("init-probe-offers");
    let (base, listener) = bind_ipv4_in_pool(12, 1);
    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let base_s = base.to_string();

    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--port-start",
        &base_s,
        "--range-size",
        "4",
        "--range-count",
        "2",
        "--probe-ports",
    ]);
    assert_eq!(code, 0, "{err}");

    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(stack.join(CONFIG_NAME)).unwrap()).unwrap();
    assert_eq!(
        cfg,
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": true,
            "seed_files": [],
            "source_repo": null,
            "version": 1,
        })
    );

    // Without passing --probe-ports on `claim`, stack config probe_ports=true skips range 0!
    let (code, out, err) = run_cli(&["claim", "holder", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&format!("ports {}-{}", base + 4, base + 7)),
        "config probe_ports=true should skip range 0: {out}"
    );

    // Queue `waiter` with `--wait`: range 0 is free in the registry, but occupied on
    // loopback, so `waiter` must queue cleanly rather than refusing!
    let (code, out, err) = run_cli(&["claim", "waiter", "--stack", &stack_s, "--wait"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("waiting waiter"), "{out}");

    // Hold range 1 on loopback too before releasing `holder`, so NEITHER range 0 nor
    // range 1 is loopback-free when `holder` releases!
    let listener_r1 = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, base + 4)).unwrap();
    let (code, out, err) = run_cli(&["release", "holder", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("released holder"), "{out}");
    assert!(
        err.is_empty(),
        "skipping a wait due to host port occupation must NOT emit a SY-25 furnishing failure: {err}"
    );
    let reg = load_registry(&stack).unwrap();
    assert!(reg.claims.is_empty());
    assert_eq!(reg.waits.len(), 1);
    assert_eq!(reg.waits[0].agent, "waiter");

    // Drop `listener_r1` (keeping `listener` on range 0), claim `temp` (lands on range 1),
    // then release `temp`: `serve_offers` skips occupied range 0 and furnishes `waiter` on range 1!
    drop(listener_r1);
    let (code, _, err) = run_cli(&["claim", "temp", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["release", "temp", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    assert!(err.is_empty(), "{err}");

    let reg = load_registry(&stack).unwrap();
    assert!(reg.waits.is_empty());
    let waiter_rec = &reg.claims["waiter"];
    assert_eq!(
        (waiter_rec.port_start, waiter_rec.port_end),
        ((base + 4) as i64, (base + 7) as i64)
    );
    drop(listener);
}

#[test]
fn prober_seam_and_routed_waiting_host_port_refusals_preserve_contracts() {
    let tmp = temp_dir("prober-seam-routed");
    let stack = tmp.join("stack");
    init_stack_with_options(
        &stack,
        24_000,
        5,
        3,
        &InitOptions {
            probe_ports: true,
            ..InitOptions::default()
        },
    )
    .unwrap();

    // Mark range 0 (24000) and range 1 (24005) busy via PortProber seam; range 2 (24010..=24014) free.
    let prober = BusySetProber {
        busy: BTreeSet::from([24_002, 24_005]),
    };
    let alpha = claim_with_prober(&stack, "alpha", ClaimOptions::default(), &prober).unwrap();
    assert_eq!((alpha.port_start, alpha.port_end), (24_010, 24_014));

    // Now range 2 is held in registry and ranges 0 & 1 are busy in prober.
    let before = snapshot(&stack);
    let err = claim_with_prober(&stack, "beta", ClaimOptions::default(), &prober)
        .expect_err("all remaining registry ranges have busy host ports");
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.message().contains("host loopback ports are occupied"),
        "{}",
        err.message()
    );
    assert_eq!(snapshot(&stack), before);

    // Compare against a stack with genuinely zero free registry ranges: exact landed wording stands!
    let full_stack = tmp.join("full_stack");
    init_stack(&full_stack, 25_000, 5, 1).unwrap();
    switchyard::claim(&full_stack, "sole").unwrap();
    let pool_err = claim_with_prober(
        &full_stack,
        "second",
        ClaimOptions { probe_ports: true },
        &BusySetProber {
            busy: BTreeSet::new(),
        },
    )
    .expect_err("exhausted registry");
    assert_eq!(
        pool_err.message(),
        "the port pool is exhausted, every range is held"
    );

    // Routed claim when real loopback ports block all ranges vs block only `standard`:
    let (rbase, r_listener0) = bind_ipv4_in_pool(12, 0);
    let r_listener1 = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, rbase + 4)).unwrap();
    let rstack = tmp.join("rstack");
    init_stack(&rstack, rbase as i128, 4, 2).unwrap();

    // Phase 1: both ranges (0 and 1) have bound host ports -> 0 consultations, routing log records "pool".
    let judge = StubJudge::new("standard", "7");
    let r_err = claim_routed_with_options(
        &rstack,
        "routed-a",
        "Run app and worker.",
        Profile::Lean,
        Some(&judge),
        ClaimOptions { probe_ports: true },
    )
    .expect_err("all ranges busy on host");
    assert_eq!(r_err.exit_code(), 1);
    assert!(
        r_err.message().contains("host loopback ports are occupied"),
        "{}",
        r_err.message()
    );
    assert_eq!(judge.calls.borrow().len(), 0);
    let lines: Vec<serde_json::Value> = fs::read_to_string(rstack.join(ROUTING_LOG_NAME))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["fallback"], "no consultation");
    assert_eq!(lines[0]["refusal"], "pool");

    // Free range 1 (`r_listener1`), keeping `r_listener0` on range 0:
    // Now `lean` (1 range) CAN be furnished, so Phase 1 consults the judge!
    // Judge settles `standard` (2 ranges), which fails in Phase 3 because range 0 is bound on host!
    drop(r_listener1);
    let r_err2 = claim_routed_with_options(
        &rstack,
        "routed-b",
        "Run app and worker and cache.",
        Profile::Lean,
        Some(&judge),
        ClaimOptions { probe_ports: true },
    )
    .expect_err("standard blocked by range 0 host listener");
    assert_eq!(r_err2.exit_code(), 1);
    assert!(
        r_err2
            .message()
            .contains("profile standard cannot be furnished, host loopback ports are occupied"),
        "{}",
        r_err2.message()
    );
    assert_eq!(judge.calls.borrow().len(), 1);
    let lines: Vec<serde_json::Value> = fs::read_to_string(rstack.join(ROUTING_LOG_NAME))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["verdict"], "standard");
    assert_eq!(lines[1]["refusal"], "pool");

    // With `wait = true`, the same `standard` routed claim queues cleanly and its
    // scoring vignette reports registry free_runs (`are 2.`) per SY-20!
    let judge_wait = StubJudge::new("standard", "8");
    let outcome = claim_routed_waiting_with_options(
        &rstack,
        "routed-waiter",
        "Run app and worker and cache.",
        Profile::Lean,
        Some(300),
        Some(&judge_wait),
        ClaimOptions { probe_ports: true },
    )
    .unwrap();
    match outcome {
        ClaimOutcome::Wait(wait) => {
            assert_eq!(wait.agent, "routed-waiter");
            assert_eq!(wait.profile, "standard");
            assert_eq!(wait.score, Some(8));
        }
        ClaimOutcome::Claim(rec) => panic!("expected wait, got claim {rec:?}"),
    }
    let calls = judge_wait.calls.borrow();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].contains("The free runs standing in the pool, in ranges, are 2."),
        "SY-20 scoring vignette must derive purely from registry state: {}",
        calls[1]
    );

    // Also verify `release_report_with_prober` skips a busy range and furnishes on a free one.
    let mut release_stack_cfg = tmp.join("rel_prober");
    init_stack_with_options(
        &release_stack_cfg,
        26_000,
        4,
        2,
        &InitOptions {
            probe_ports: true,
            ..InitOptions::default()
        },
    )
    .unwrap();
    release_stack_cfg = resolve_path(&release_stack_cfg);
    let empty_prober = BusySetProber {
        busy: BTreeSet::new(),
    };
    let h0 = claim_with_prober(
        &release_stack_cfg,
        "h0",
        ClaimOptions::default(),
        &empty_prober,
    )
    .unwrap();
    let h1 = claim_with_prober(
        &release_stack_cfg,
        "h1",
        ClaimOptions::default(),
        &empty_prober,
    )
    .unwrap();
    assert_eq!(h0.port_start, 26_000);
    assert_eq!(h1.port_start, 26_004);
    let w = claim_waiting_with_options(
        &release_stack_cfg,
        "w0",
        None,
        None,
        ClaimOptions::default(),
    )
    .unwrap();
    assert!(matches!(w, ClaimOutcome::Wait(_)));
    let busy_first = BusySetProber {
        busy: BTreeSet::from([26_000]),
    };
    let rep0 = release_report_with_prober(
        &release_stack_cfg,
        "h0",
        ReleaseOptions::default(),
        &busy_first,
    )
    .unwrap();
    assert!(rep0.furnishing_failures.is_empty());
    assert_eq!(load_registry(&release_stack_cfg).unwrap().waits.len(), 1);

    let rep1 = release_report_with_prober(
        &release_stack_cfg,
        "h1",
        ReleaseOptions::default(),
        &busy_first,
    )
    .unwrap();
    assert!(rep1.furnishing_failures.is_empty());
    let reg_after = load_registry(&release_stack_cfg).unwrap();
    assert!(reg_after.waits.is_empty());
    assert_eq!(reg_after.claims["w0"].port_start, 26_004);
    assert!(release_stack_cfg.join(QUEUE_LOG_NAME).is_file());
    drop(r_listener0);
}

// =====================================================================
// SY-31: Enriched `.env.switchyard` & `--env-template`
// =====================================================================

#[test]
fn default_env_switchyard_preserves_first_eight_lines_and_caps_indexed_ports_at_40() {
    let tmp = temp_dir("env-default-and-cap");
    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");

    let resolved_wt = resolve_path(&stack.join("worktrees/alpha"));
    let wt_abs = resolved_wt.to_str().unwrap();
    let env_text = fs::read_to_string(resolved_wt.join(ENV_NAME)).unwrap();
    let expected = format!(
        "# Written by switchyard. This file dies with the worktree.\n\
SWITCHYARD_AGENT=alpha\n\
SWITCHYARD_BRANCH=switchyard/alpha\n\
SWITCHYARD_WORKTREE=worktrees/alpha\n\
SWITCHYARD_PORT_START=20000\n\
SWITCHYARD_PORT_END=20009\n\
PORT=20000\n\
SWITCHYARD_DATABASE=switchyard_alpha\n\
DATABASE_URL=postgres://127.0.0.1:20000/switchyard_alpha\n\
SWITCHYARD_WORKTREE_ABS={wt_abs}\n\
SWITCHYARD_PORT_COUNT=10\n\
SWITCHYARD_PORT_0=20000\n\
SWITCHYARD_PORT_1=20001\n\
SWITCHYARD_PORT_2=20002\n\
SWITCHYARD_PORT_3=20003\n\
SWITCHYARD_PORT_4=20004\n\
SWITCHYARD_PORT_5=20005\n\
SWITCHYARD_PORT_6=20006\n\
SWITCHYARD_PORT_7=20007\n\
SWITCHYARD_PORT_8=20008\n\
SWITCHYARD_PORT_9=20009\n\
SQLITE_PATH={wt_abs}/.switchyard.db\n"
    );
    assert_eq!(env_text, expected);

    // Verify 40-port cap on a 50-port span (`SWITCHYARD_PORT_0`..`SWITCHYARD_PORT_39` only).
    let wide = tmp.join("wide");
    let wide_s = wide.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &wide_s,
        "--port-start",
        "21000",
        "--range-size",
        "50",
        "--range-count",
        "2",
    ]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &wide_s]);
    assert_eq!(code, 0, "{err}");
    let wide_env = fs::read_to_string(wide.join("worktrees/alpha").join(ENV_NAME)).unwrap();
    assert!(wide_env.contains("\nSWITCHYARD_PORT_COUNT=50\n"));
    assert!(wide_env.contains("\nSWITCHYARD_PORT_0=21000\n"));
    assert!(wide_env.contains("\nSWITCHYARD_PORT_39=21039\n"));
    assert!(!wide_env.contains("SWITCHYARD_PORT_40="));
}

#[test]
fn env_template_renders_placeholders_overrides_in_place_and_serves_offers() {
    let tmp = temp_dir("env-template-render");
    let tpl = tmp.join("custom.env.tpl");
    fs::write(
        &tpl,
        "# leading comment\r\n\
         \r\n\
         \x20\x20# indented comment\n\
         PORT={{port_2}}\r\n\
         DATABASE_URL=postgres://localhost:{{port_1}}/{{database}}?application_name={{agent}}\n\
         SQLITE_PATH={{worktree_abs}}/data/{{agent}}.sqlite\n\
         API_ORIGIN=http://127.0.0.1:{{port_0}}\n\
         META={{agent}}|{{branch}}|{{worktree}}|{{port_start}}-{{port_end}}|{{port_count}}\r\n",
    )
    .unwrap();

    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let tpl_s = tpl.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--range-size",
        "3",
        "--range-count",
        "1",
        "--env-template",
        &tpl_s,
    ]);
    assert_eq!(code, 0, "{err}");

    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(stack.join(CONFIG_NAME)).unwrap()).unwrap();
    assert_eq!(cfg["env_template"].as_array().unwrap().len(), 5);
    assert_eq!(cfg["env_template"][0]["key"], "PORT");
    assert_eq!(cfg["env_template"][0]["template"], "{{port_2}}");

    // Claim `alpha` directly and queue `beta` so `serve_offers` also renders the template!
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "beta", "--stack", &stack_s, "--wait"]);
    assert_eq!(code, 0, "{err}");

    let alpha_wt = resolve_path(&stack.join("worktrees/alpha"));
    let alpha_abs = alpha_wt.to_str().unwrap();
    let alpha_env = fs::read_to_string(alpha_wt.join(ENV_NAME)).unwrap();
    let expected_alpha = format!(
        "# Written by switchyard. This file dies with the worktree.\n\
SWITCHYARD_AGENT=alpha\n\
SWITCHYARD_BRANCH=switchyard/alpha\n\
SWITCHYARD_WORKTREE=worktrees/alpha\n\
SWITCHYARD_PORT_START=20000\n\
SWITCHYARD_PORT_END=20002\n\
PORT=20002\n\
SWITCHYARD_DATABASE=switchyard_alpha\n\
DATABASE_URL=postgres://localhost:20001/switchyard_alpha?application_name=alpha\n\
SWITCHYARD_WORKTREE_ABS={alpha_abs}\n\
SWITCHYARD_PORT_COUNT=3\n\
SWITCHYARD_PORT_0=20000\n\
SWITCHYARD_PORT_1=20001\n\
SWITCHYARD_PORT_2=20002\n\
SQLITE_PATH={alpha_abs}/data/alpha.sqlite\n\
API_ORIGIN=http://127.0.0.1:20000\n\
META=alpha|switchyard/alpha|worktrees/alpha|20000-20002|3\n"
    );
    assert_eq!(alpha_env, expected_alpha);

    // Release `alpha` -> `beta` is furnished by `serve_offers` with the same template!
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_s, "--safe"]);
    assert_eq!(code, 0, "{err}");
    let beta_wt = resolve_path(&stack.join("worktrees/beta"));
    let beta_abs = beta_wt.to_str().unwrap();
    let beta_env = fs::read_to_string(beta_wt.join(ENV_NAME)).unwrap();
    assert!(beta_env.contains("\nPORT=20002\n"));
    assert!(beta_env.contains(&format!("\nSQLITE_PATH={beta_abs}/data/beta.sqlite\n")));
    assert!(beta_env.ends_with("META=beta|switchyard/beta|worktrees/beta|20000-20002|3\n"));

    // Comment-only template file stores `"env_template": []` in switchyard-config.json.
    let empty_tpl = tmp.join("comments_only.tpl");
    fs::write(&empty_tpl, "# only comments\n   \n").unwrap();
    let empty_stack = tmp.join("empty_tpl_stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &empty_stack.to_string_lossy(),
        "--env-template",
        &empty_tpl.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");
    let empty_cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(empty_stack.join(CONFIG_NAME)).unwrap()).unwrap();
    assert_eq!(empty_cfg["env_template"], serde_json::json!([]));
}

#[test]
fn env_template_refuses_invalid_files_reserved_keys_duplicates_and_bad_placeholders() {
    let tmp = temp_dir("env-template-invalid");

    // 1. Missing file, directory, symlink, non-UTF-8, and > 64 KiB file.
    let missing = tmp.join("does_not_exist.tpl");
    let as_dir = tmp.join("dir.tpl");
    fs::create_dir(&as_dir).unwrap();
    let real_tpl = tmp.join("real.tpl");
    fs::write(&real_tpl, "APP_PORT={{port_0}}\n").unwrap();
    let sym_tpl = tmp.join("sym.tpl");
    symlink(&real_tpl, &sym_tpl).unwrap();
    let non_utf8 = tmp.join("bad_utf8.tpl");
    fs::write(&non_utf8, b"APP_PORT=\xff\n").unwrap();
    let oversized = tmp.join("oversized.tpl");
    fs::write(&oversized, vec![b'#'; 65_537]).unwrap();

    for (idx, bad_path) in [&missing, &as_dir, &sym_tpl, &non_utf8, &oversized]
        .into_iter()
        .enumerate()
    {
        let stack = tmp.join(format!("bad-file-{idx}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--env-template",
            &bad_path.to_string_lossy(),
        ]);
        assert_eq!(code, 1, "case {idx} should fail at exit 1");
        assert!(!err.is_empty());
        assert!(!stack.exists(), "case {idx} left stack residue");
    }

    // 2. Invalid syntax, reserved SWITCHYARD_ keys, duplicate keys, and bad placeholders.
    let long_key_129 = format!("{}=ok\n", "K".repeat(129));
    let bad_contents: Vec<(&str, &str)> = vec![
        ("missing-equals", "JUST_A_KEY\n"),
        ("empty-key", "=value\n"),
        ("digit-start", "9PORT={{port_0}}\n"),
        ("hyphen-key", "APP-PORT={{port_0}}\n"),
        ("space-around-key", "APP PORT={{port_0}}\n"),
        ("key-129-bytes", &long_key_129),
        ("switchyard-agent", "SWITCHYARD_AGENT=hijack\n"),
        ("switchyard-custom", "SWITCHYARD_EXTRA={{port_0}}\n"),
        ("switchyard-bare-underscore", "SWITCHYARD_=x\n"),
        (
            "duplicate-key",
            "APP_PORT={{port_0}}\nAPP_PORT={{port_1}}\n",
        ),
        ("unclosed-open", "APP_URL=http://localhost:{{port_0\n"),
        ("stray-close", "APP_URL=http://localhost:port_0}}\n"),
        ("close-before-open", "APP_URL=}}http://{{port_0}}\n"),
        ("nested-open", "APP_URL={{{{agent}}}}\n"),
        ("unknown-placeholder", "APP_URL={{hostname}}\n"),
        ("port-40", "APP_PORT={{port_40}}\n"),
        ("port-leading-zero", "APP_PORT={{port_00}}\n"),
        ("embedded-cr", "APP_URL=a\rb\n"),
        ("trailing-bare-cr-eof", "APP_PORT=20000\r"),
    ];
    for (label, body) in bad_contents {
        let path = tmp.join(format!("bad-{label}.tpl"));
        fs::write(&path, body).unwrap();
        let stack = tmp.join(format!("stack-{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--env-template",
            &path.to_string_lossy(),
        ]);
        assert_eq!(code, 1, "{label} should exit 1, stderr: {err}");
        assert!(!stack.exists(), "{label} must not create stack directory");
    }

    // Boundary check: 128-byte key and non-SWITCHYARD_ prefix (`SWITCHYARD=ok` and `MY_SWITCHYARD_X=ok`) succeed!
    let key_128 = "A".repeat(128);
    let ok_tpl = tmp.join("boundary_ok.tpl");
    fs::write(
        &ok_tpl,
        format!(
            "{key_128}={{{{agent}}}}\nSWITCHYARD=allowed\nMY_SWITCHYARD_PORT={{{{port_39}}}}\n"
        ),
    )
    .unwrap();
    let ok_stack = tmp.join("boundary_ok_stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &ok_stack.to_string_lossy(),
        "--range-size",
        "40",
        "--range-count",
        "1",
        "--env-template",
        &ok_tpl.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &ok_stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    let ok_env = fs::read_to_string(ok_stack.join("worktrees/alpha").join(ENV_NAME)).unwrap();
    assert!(ok_env.contains(&format!("\n{key_128}=alpha\n")), "{ok_env}");
    assert!(ok_env.contains("\nSWITCHYARD=allowed\n"), "{ok_env}");
    assert!(ok_env.contains("\nMY_SWITCHYARD_PORT=20039\n"), "{ok_env}");
}

#[test]
fn out_of_range_port_placeholder_fails_closed_and_rolls_back_worktree_and_branch() {
    let tmp = temp_dir("env-template-oob-rollback");
    let tpl = tmp.join("six_ports.tpl");
    fs::write(&tpl, "SIXTH_PORT={{port_5}}\n").unwrap();

    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--range-size",
        "3",
        "--range-count",
        "2",
        "--env-template",
        &tpl.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");

    // Plain claim draws 3 ports (0..=2); `{{port_5}}` must fail closed and roll back!
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_s]);
    assert_eq!(code, 1);
    assert!(
        err.contains("{{port_5}}") && err.contains("3 ports"),
        "expected out-of-range port placeholder refusal: {err}"
    );
    assert!(!stack.join("worktrees/alpha").exists());
    assert_eq!(
        git_code(
            &stack.join(REPO_NAME),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/switchyard/alpha"
            ]
        ),
        1,
        "rollback must delete refs/heads/switchyard/alpha"
    );
    assert!(load_registry(&stack).unwrap().claims.is_empty());

    // Claiming `alpha` again with `--profile standard` (6 ports, 0..=5) succeeds cleanly!
    let (code, _, err) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        &stack_s,
        "--profile",
        "standard",
    ]);
    assert_eq!(code, 0, "{err}");
    let env_text = fs::read_to_string(stack.join("worktrees/alpha").join(ENV_NAME)).unwrap();
    assert!(env_text.contains("\nSIXTH_PORT=20005\n"), "{env_text}");

    // Queue a `lean` waiter (3 ports) and a `standard` waiter (6 ports), then release `alpha`:
    // `lean` waiter fails `{{port_5}}` (reported as SY-25 furnishing failure, rolled back),
    // and `standard` waiter behind it is furnished cleanly!
    let (code, _, err) = run_cli(&[
        "claim",
        "short-waiter",
        "--stack",
        &stack_s,
        "--profile",
        "lean",
        "--wait",
    ]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&[
        "claim",
        "wide-waiter",
        "--stack",
        &stack_s,
        "--profile",
        "standard",
        "--wait",
    ]);
    assert_eq!(code, 0, "{err}");

    let (code, out, err) = run_cli(&["release", "alpha", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("released alpha"));
    assert!(
        err.contains("short-waiter") && err.contains("{{port_5}}"),
        "SY-25 furnishing failure expected on stderr: {err}"
    );
    assert!(!stack.join("worktrees/short-waiter").exists());
    let reg = load_registry(&stack).unwrap();
    assert!(reg.claims.contains_key("wide-waiter"));
    assert_eq!(reg.waits.len(), 1);
    assert_eq!(reg.waits[0].agent, "short-waiter");
}

// =====================================================================
// SY-31: Symlink-Safe `.worktreeinclude` Seeding, `release --safe`, `doctor`
// =====================================================================

#[test]
fn worktreeinclude_seeds_untracked_ignored_nested_executable_and_tracked_overwrites() {
    let tmp = temp_dir("wti-happy-path");
    let src = init_source_repo(&tmp);

    fs::write(src.join(".gitignore"), ".env.local\nignored_dir/\n").unwrap();
    fs::create_dir_all(src.join("config")).unwrap();
    fs::write(src.join("config/app.toml"), "mode = \"committed\"\n").unwrap();
    git(&src, &["add", ".gitignore", "config/app.toml"]);
    git(
        &src,
        &["commit", "-q", "-m", "add gitignore and app config"],
    );

    // Overwrite tracked `config/app.toml` in the source working tree without committing,
    // plus create untracked, gitignored, nested gitignored, and executable seed files.
    fs::write(src.join("config/app.toml"), "mode = \"seeded-override\"\n").unwrap();
    fs::write(src.join("local.cfg"), "untracked=1\n").unwrap();
    fs::write(src.join(".env.local"), "DEV_TOKEN=synthetic-token\n").unwrap();
    fs::create_dir_all(src.join("ignored_dir/nested")).unwrap();
    fs::write(src.join("ignored_dir/nested/key.pem"), "SYNTHETIC-KEY\n").unwrap();
    fs::create_dir_all(src.join("bin")).unwrap();
    let script_path = src.join("bin/bootstrap.sh");
    fs::write(&script_path, "#!/bin/sh\necho seeded\n").unwrap();
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();

    // Write `.worktreeinclude` in unsorted order with comments and CRLF endings.
    fs::write(
        src.join(WORKTREEINCLUDE_NAME),
        "# local files to seed into each agent worktree\r\n\
         local.cfg\r\n\
         .env.local\n\
         \n\
         ignored_dir/nested/key.pem\r\n\
         bin/bootstrap.sh\n\
         config/app.toml\n",
    )
    .unwrap();

    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--from",
        &src.to_string_lossy(),
        "--range-count",
        "1",
    ]);
    assert_eq!(code, 0, "{err}");

    // Verify `switchyard-config.json` records `seed_files` in strictly ascending path order
    // with exact lowercase 64-hex SHA-256 digests!
    let cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(stack.join(CONFIG_NAME)).unwrap()).unwrap();
    let seeds = cfg["seed_files"].as_array().unwrap();
    let paths: Vec<&str> = seeds.iter().map(|s| s["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        vec![
            ".env.local",
            "bin/bootstrap.sh",
            "config/app.toml",
            "ignored_dir/nested/key.pem",
            "local.cfg",
        ]
    );
    assert_eq!(
        seeds[0]["sha256"].as_str().unwrap(),
        sha256_hex(b"DEV_TOKEN=synthetic-token\n")
    );

    // Verify `<stack>/seed/` permissions: 0700 on executable, 0600 on regular non-executable.
    let seed_exec_mode = fs::symlink_metadata(stack.join(SEED_DIR_NAME).join("bin/bootstrap.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let seed_plain_mode = fs::symlink_metadata(stack.join(SEED_DIR_NAME).join(".env.local"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(seed_exec_mode, 0o700);
    assert_eq!(seed_plain_mode, 0o600);

    // Mutate source repo's `.env.local` AFTER `init` to prove claims materialize the
    // immutable snapshot captured at `init` time!
    fs::write(src.join(".env.local"), "DEV_TOKEN=changed-after-init\n").unwrap();

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = run_cli(&["claim", "beta", "--stack", &stack_s, "--wait"]);
    assert_eq!(code, 0, "{err}");

    let alpha_wt = stack.join(WORKTREES_NAME).join("alpha");
    assert_eq!(
        fs::read_to_string(alpha_wt.join(".env.local")).unwrap(),
        "DEV_TOKEN=synthetic-token\n"
    );
    assert_eq!(
        fs::read_to_string(alpha_wt.join("config/app.toml")).unwrap(),
        "mode = \"seeded-override\"\n"
    );
    assert_eq!(
        fs::read_to_string(alpha_wt.join("ignored_dir/nested/key.pem")).unwrap(),
        "SYNTHETIC-KEY\n"
    );
    assert_eq!(
        fs::read_to_string(alpha_wt.join("local.cfg")).unwrap(),
        "untracked=1\n"
    );
    let wt_exec_mode = fs::symlink_metadata(alpha_wt.join("bin/bootstrap.sh"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(wt_exec_mode, 0o700);

    // `release alpha --safe` must treat all 5 unmodified seeded files as clean, AND
    // `serve_offers` must materialize all 5 seeded files into `beta`'s worktree!
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 0,
        "unmodified seeded files must pass release --safe: {err}"
    );

    let beta_wt = stack.join(WORKTREES_NAME).join("beta");
    assert_eq!(
        fs::read_to_string(beta_wt.join(".env.local")).unwrap(),
        "DEV_TOKEN=synthetic-token\n"
    );
    assert_eq!(
        fs::read_to_string(beta_wt.join("config/app.toml")).unwrap(),
        "mode = \"seeded-override\"\n"
    );
    assert_eq!(
        fs::read_to_string(beta_wt.join("ignored_dir/nested/key.pem")).unwrap(),
        "SYNTHETIC-KEY\n"
    );

    // Source repo without `.worktreeinclude` creates NO `<stack>/seed` directory.
    let bare_src = tmp.join("bare_src_wrap");
    fs::create_dir_all(&bare_src).unwrap();
    let clean_src = init_source_repo(&bare_src);
    let no_seed_stack = tmp.join("no_seed_stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &no_seed_stack.to_string_lossy(),
        "--from",
        &clean_src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(!no_seed_stack.join(SEED_DIR_NAME).exists());
}

#[test]
fn worktreeinclude_init_refuses_symlinks_traversals_globs_reserved_paths_and_duplicates() {
    let tmp = temp_dir("wti-init-refusals");
    let src = init_source_repo(&tmp);
    fs::write(src.join("valid.env"), "OK=1\n").unwrap();
    fs::create_dir_all(src.join("real_dir")).unwrap();

    // Outside secret target that symlink attacks attempt to exfiltrate.
    let outside_dir = tmp.join("outside_secrets");
    fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = outside_dir.join("secret.txt");
    fs::write(&outside_file, "TOP-SECRET\n").unwrap();

    // 1. `.worktreeinclude` is itself a symlink -> refused!
    symlink(&outside_file, src.join(WORKTREEINCLUDE_NAME)).unwrap();
    let s_sym = tmp.join("stack-wti-symlink");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &s_sym.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(!s_sym.exists());
    fs::remove_file(src.join(WORKTREEINCLUDE_NAME)).unwrap();

    // 2. `.worktreeinclude` is a directory -> refused!
    fs::create_dir(src.join(WORKTREEINCLUDE_NAME)).unwrap();
    let s_dir = tmp.join("stack-wti-dir");
    let (code, _, _) = run_cli(&[
        "init",
        "--stack",
        &s_dir.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1);
    assert!(!s_dir.exists());
    fs::remove_dir(src.join(WORKTREEINCLUDE_NAME)).unwrap();

    // 3. Source symlink leaf and source symlink parent directory!
    symlink(&outside_file, src.join("leaf_link.env")).unwrap();
    symlink(&outside_dir, src.join("parent_link")).unwrap();

    let bad_lines: Vec<(&str, &str)> = vec![
        ("parent-dotdot", "../outside_secrets/secret.txt\n"),
        (
            "nested-dotdot",
            "real_dir/../../outside_secrets/secret.txt\n",
        ),
        ("dot-component", "./valid.env\n"),
        ("leading-slash", "/valid.env\n"),
        ("trailing-slash", "real_dir/\n"),
        ("double-slash", "real_dir//valid.env\n"),
        ("backslash", "real_dir\\valid.env\n"),
        ("colon", "C:valid.env\n"),
        ("glob-star", "*.env\n"),
        ("glob-qmark", "valid?.env\n"),
        ("glob-bracket", "valid[0].env\n"),
        ("glob-brace", "{valid}.env\n"),
        ("dot-git-lower", ".git/config\n"),
        ("dot-git-mixed", "sub/.GiT/hooks\n"),
        ("reserved-env-lower", ".env.switchyard\n"),
        ("reserved-env-upper", ".ENV.SWITCHYARD\n"),
        ("reserved-env-dir-lower", ".env.switchyard/local.env\n"),
        ("reserved-env-dir-upper", ".ENV.SWITCHYARD/local.env\n"),
        ("reserved-hooks", ".switchyard-empty-hooks/post-checkout\n"),
        ("trailing-bare-cr-eof", "valid.env\r"),
        ("duplicate-entry", "valid.env\nvalid.env\n"),
        ("missing-file", "does_not_exist.env\n"),
        ("directory-leaf", "real_dir\n"),
        ("symlink-leaf", "leaf_link.env\n"),
        ("symlink-parent", "parent_link/secret.txt\n"),
    ];

    for (label, content) in bad_lines {
        fs::write(src.join(WORKTREEINCLUDE_NAME), content).unwrap();
        let stack = tmp.join(format!("stack-{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--from",
            &src.to_string_lossy(),
        ]);
        assert_eq!(code, 1, "{label} must refuse at exit 1, stderr: {err}");
        assert!(!stack.exists(), "{label} left stack residue");
    }

    // 4. Preexisting `<stack>/seed` directory is refused without deleting it!
    fs::write(src.join(WORKTREEINCLUDE_NAME), "valid.env\n").unwrap();
    let pre_seed_stack = tmp.join("pre_seed_stack");
    fs::create_dir_all(pre_seed_stack.join(SEED_DIR_NAME)).unwrap();
    fs::write(
        pre_seed_stack.join(SEED_DIR_NAME).join("keep.txt"),
        "keep\n",
    )
    .unwrap();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &pre_seed_stack.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "{err}");
    assert!(pre_seed_stack
        .join(SEED_DIR_NAME)
        .join("keep.txt")
        .is_file());
    assert!(!pre_seed_stack.join(LOCK_NAME).exists());

    // Bare `init` (no planned seeds) ignores a preexisting `<stack>/seed` entry and preserves it!
    let bare_with_seed = tmp.join("bare_with_seed");
    fs::create_dir_all(bare_with_seed.join(SEED_DIR_NAME)).unwrap();
    fs::write(
        bare_with_seed.join(SEED_DIR_NAME).join("keep.txt"),
        "keep\n",
    )
    .unwrap();
    let (code, _, err) = run_cli(&["init", "--stack", &bare_with_seed.to_string_lossy()]);
    assert_eq!(
        code, 0,
        "bare init with no seeds must allow preexisting seed/: {err}"
    );
    assert!(bare_with_seed
        .join(SEED_DIR_NAME)
        .join("keep.txt")
        .is_file());
}

#[test]
fn claim_refuses_tracked_worktree_symlinks_and_tampered_seed_store_with_rollback() {
    let tmp = temp_dir("claim-symlink-defense");
    let victim_dir = tmp.join("victim_dir");
    fs::create_dir_all(&victim_dir).unwrap();
    let victim_leaf = tmp.join("victim_leaf.txt");
    fs::write(&victim_leaf, "ORIGINAL-VICTIM\n").unwrap();

    let src = init_source_repo(&tmp);

    // Create branch `evil-parent` where `sub` is a tracked git symlink pointing to `victim_dir`.
    git(&src, &["checkout", "-q", "-b", "evil-parent"]);
    symlink(&victim_dir, src.join("sub")).unwrap();
    git(&src, &["add", "sub"]);
    git(&src, &["commit", "-q", "-m", "track symlink directory"]);

    // Create branch `evil-leaf` where `secret.env` is a tracked git symlink pointing to `victim_leaf`.
    git(&src, &["checkout", "-q", "-b", "evil-leaf", "main"]);
    symlink(&victim_leaf, src.join("secret.env")).unwrap();
    git(&src, &["add", "secret.env"]);
    git(&src, &["commit", "-q", "-m", "track symlink leaf"]);

    // Return source working tree to `main` where `sub/payload.txt` and `secret.env` are real files!
    git(&src, &["checkout", "-q", "main"]);
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/payload.txt"), "SEED-PAYLOAD\n").unwrap();
    fs::write(src.join("secret.env"), "SEED-LEAF\n").unwrap();
    fs::write(
        src.join(WORKTREEINCLUDE_NAME),
        "sub/payload.txt\nsecret.env\n",
    )
    .unwrap();

    // Stack 1: `--branch evil-parent`. `init` succeeds from real files on `main` working tree,
    // and `claim` checks out `evil-parent` where `worktrees/alpha/sub` is a symlink!
    let stack_parent = tmp.join("stack_parent");
    let stack_parent_s = stack_parent.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_parent_s,
        "--from",
        &src.to_string_lossy(),
        "--branch",
        "evil-parent",
    ]);
    assert_eq!(code, 0, "{err}");

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_parent_s]);
    assert_eq!(code, 1);
    assert!(
        err.contains("symlink"),
        "expected symlink parent refusal, got: {err}"
    );
    assert!(
        !victim_dir.join("payload.txt").exists(),
        "seed write MUST NOT traverse tracked symlink parent into external directory!"
    );
    assert!(!stack_parent.join("worktrees/alpha").exists());
    assert_eq!(
        git_code(
            &stack_parent.join(REPO_NAME),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/switchyard/alpha"
            ]
        ),
        1
    );

    // Stack 2: `--branch evil-leaf`. `claim` checks out `evil-leaf` where `secret.env` is a symlink!
    let stack_leaf = tmp.join("stack_leaf");
    let stack_leaf_s = stack_leaf.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_leaf_s,
        "--from",
        &src.to_string_lossy(),
        "--branch",
        "evil-leaf",
    ]);
    assert_eq!(code, 0, "{err}");

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_leaf_s]);
    assert_eq!(code, 1);
    assert!(
        err.contains("non-regular entry"),
        "expected non-regular destination refusal, got: {err}"
    );
    assert_eq!(
        fs::read_to_string(&victim_leaf).unwrap(),
        "ORIGINAL-VICTIM\n",
        "victim file outside worktree must remain untouched"
    );
    assert!(!stack_leaf.join("worktrees/alpha").exists());

    // Stack 3: `--branch main`, then tamper `<stack>/seed/secret.env` on disk -> SHA-256 mismatch!
    let stack_tamper = tmp.join("stack_tamper");
    let stack_tamper_s = stack_tamper.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_tamper_s,
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");
    fs::write(
        stack_tamper.join(SEED_DIR_NAME).join("secret.env"),
        "TAMPERED-BYTES\n",
    )
    .unwrap();

    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &stack_tamper_s]);
    assert_eq!(code, 1);
    assert!(
        err.contains("SHA-256"),
        "expected SHA-256 verification refusal, got: {err}"
    );
    assert!(!stack_tamper.join("worktrees/alpha").exists());
    assert!(load_registry(&stack_tamper).unwrap().claims.is_empty());
}

#[test]
fn safe_release_and_doctor_protect_modified_or_staged_seeds_and_clean_unmodified_orphans() {
    let tmp = temp_dir("safe-release-doctor-seeds");
    let src = init_source_repo(&tmp);

    fs::write(src.join(".gitignore"), ".env.local\nsecrets/\n*.ignored\n").unwrap();
    fs::write(src.join("tracked.cfg"), "base\n").unwrap();
    git(&src, &["add", ".gitignore", "tracked.cfg"]);
    git(&src, &["commit", "-q", "-m", "add tracked and gitignore"]);

    fs::write(src.join("tracked.cfg"), "seeded-tracked\n").unwrap();
    fs::write(src.join("untracked.cfg"), "seeded-untracked\n").unwrap();
    fs::write(src.join(".env.local"), "SEEDED_SECRET=1\n").unwrap();
    fs::create_dir_all(src.join("secrets/sub")).unwrap();
    fs::write(src.join("secrets/sub/api.key"), "SEEDED_API_KEY\n").unwrap();
    fs::write(
        src.join(WORKTREEINCLUDE_NAME),
        "tracked.cfg\nuntracked.cfg\n.env.local\nsecrets/sub/api.key\n",
    )
    .unwrap();

    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");

    // 1. `release --safe` refuses modified gitignored `.env.local`!
    let (code, _, err) = run_cli(&["claim", "a1", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let wt_a1 = stack.join(WORKTREES_NAME).join("a1");
    // Unrelated gitignored build artifact is still allowed by `release --safe` per SY-28,
    // whereas modifying seeded `.env.local` MUST be refused!
    fs::write(wt_a1.join("build.ignored"), "cache\n").unwrap();
    fs::write(
        wt_a1.join(".env.local"),
        "SEEDED_SECRET=modified-by-agent\n",
    )
    .unwrap();
    let (code, _, err) = run_cli(&["release", "a1", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 1,
        "modified gitignored seed file must refuse release --safe"
    );
    assert!(err.contains("uncommitted or untracked changes"), "{err}");
    // Restore `.env.local` to exact seeded content -> `release --safe` now succeeds!
    fs::write(wt_a1.join(".env.local"), "SEEDED_SECRET=1\n").unwrap();
    let (code, _, err) = run_cli(&["release", "a1", "--stack", &stack_s, "--safe"]);
    assert_eq!(code, 0, "{err}");

    // 2. `release --safe` refuses staged unmodified seed (`git add untracked.cfg`) and deleted tracked seed!
    let (code, _, err) = run_cli(&["claim", "a2", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");
    let wt_a2 = stack.join(WORKTREES_NAME).join("a2");
    git(&wt_a2, &["add", "untracked.cfg"]);
    let (code, _, _) = run_cli(&["release", "a2", "--stack", &stack_s, "--safe"]);
    assert_eq!(code, 1, "staged seed file must refuse release --safe");
    git(&wt_a2, &["reset", "-q", "HEAD", "--", "untracked.cfg"]);
    fs::set_permissions(wt_a2.join("tracked.cfg"), fs::Permissions::from_mode(0o755)).unwrap();
    let (code, _, _) = run_cli(&["release", "a2", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 1,
        "executable-bit edit on tracked seed must refuse release --safe"
    );
    fs::set_permissions(wt_a2.join("tracked.cfg"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(wt_a2.join("tracked.cfg")).unwrap();
    let (code, _, _) = run_cli(&["release", "a2", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 1,
        "deleted tracked seed file must refuse release --safe"
    );
    let (code, _, err) = run_cli(&["release", "a2", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");

    // 3. `doctor` & `doctor --fix` on orphan worktrees with seeded files:
    //    - `clean-orphan`: unmodified untracked, gitignored `.env.local`, gitignored `secrets/sub/api.key`,
    //      and unstaged `tracked.cfg` -> `orphan_worktree_dir` (clean) and removed by `--fix`!
    //    - `dirty-mod-ignored`: `.env.local` modified -> `dirty_orphan_worktree`, preserved!
    //    - `dirty-extra-in-ignored-dir`: `secrets/extra.txt` added inside gitignored `secrets/` ->
    //      `dirty_orphan_worktree`, preserved!
    //    - `dirty-staged`: `untracked.cfg` staged in index -> `dirty_orphan_worktree`, preserved!
    for agent in [
        "clean-orphan",
        "dirty-mod-ignored",
        "dirty-extra-in-ignored-dir",
        "dirty-staged",
    ] {
        let (code, _, err) = run_cli(&["claim", agent, "--stack", &stack_s]);
        assert_eq!(code, 0, "{err}");
    }
    fs::write(
        stack
            .join(WORKTREES_NAME)
            .join("dirty-mod-ignored")
            .join(".env.local"),
        "USER_EDITED=1\n",
    )
    .unwrap();
    fs::write(
        stack
            .join(WORKTREES_NAME)
            .join("dirty-extra-in-ignored-dir")
            .join("secrets/extra.txt"),
        "unsaved\n",
    )
    .unwrap();
    git(
        &stack.join(WORKTREES_NAME).join("dirty-staged"),
        &["add", "untracked.cfg"],
    );

    // Clear `claims` in registry so all four worktrees stand as orphans!
    let reg_path = stack.join(REGISTRY_NAME);
    let mut reg_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&reg_path).unwrap()).unwrap();
    reg_json["claims"] = serde_json::json!({});
    fs::write(
        &reg_path,
        serde_json::to_string_pretty(&reg_json).unwrap() + "\n",
    )
    .unwrap();

    let report = switchyard::doctor(&stack, true).unwrap();
    assert!(!report.ok, "dirty orphans must remain unfixed");
    let by_target: BTreeMap<String, (&str, bool)> = report
        .issues
        .iter()
        .filter(|i| i.target.starts_with("worktrees/"))
        .map(|i| (i.target.clone(), (i.kind.as_str(), i.fixed)))
        .collect();
    assert_eq!(
        by_target.get("worktrees/clean-orphan"),
        Some(&("orphan_worktree_dir", true))
    );
    assert_eq!(
        by_target.get("worktrees/dirty-mod-ignored"),
        Some(&("dirty_orphan_worktree", false))
    );
    assert_eq!(
        by_target.get("worktrees/dirty-extra-in-ignored-dir"),
        Some(&("dirty_orphan_worktree", false))
    );
    assert_eq!(
        by_target.get("worktrees/dirty-staged"),
        Some(&("dirty_orphan_worktree", false))
    );
    assert!(!stack.join(WORKTREES_NAME).join("clean-orphan").exists());
    assert!(stack
        .join(WORKTREES_NAME)
        .join("dirty-mod-ignored")
        .is_dir());
    assert!(stack
        .join(WORKTREES_NAME)
        .join("dirty-extra-in-ignored-dir")
        .is_dir());
    assert!(stack.join(WORKTREES_NAME).join("dirty-staged").is_dir());
}

#[test]
fn load_config_refuses_corrupt_env_template_and_seed_files_at_exit_2_without_mutation() {
    let tmp = temp_dir("corrupt-s3-config");
    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");

    let valid_sha = "a".repeat(64);
    let upper_sha = "A".repeat(64);
    let short_sha = "a".repeat(63);

    let corrupt_payloads = vec![
        serde_json::json!({
            "base_branch": "main",
            "env_template": "not-an-array",
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "PORT"}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "SWITCHYARD_PORT_0", "template": "9999"}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": [
                {"key": "APP_PORT", "template": "{{port_0}}"},
                {"key": "APP_PORT", "template": "{{port_1}}"}
            ],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "APP_PORT", "template": "{{bogus}}"}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "APP_PORT", "template": "line1\nATTACK=1"}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": null,
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [
                {"path": "b.env", "sha256": valid_sha},
                {"path": "a.env", "sha256": valid_sha}
            ],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [
                {"path": "a.env", "sha256": valid_sha},
                {"path": "a.env", "sha256": valid_sha}
            ],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [{"path": "../escape.env", "sha256": valid_sha}],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [{"path": ".env.switchyard", "sha256": valid_sha}],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [{"path": ".env.switchyard/local.env", "sha256": valid_sha}],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [{"path": "a.env", "sha256": upper_sha}],
            "source_repo": null,
            "version": 1
        }),
        serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": [{"path": "a.env", "sha256": short_sha}],
            "source_repo": null,
            "version": 1
        }),
    ];

    for (idx, payload) in corrupt_payloads.iter().enumerate() {
        fs::write(
            stack.join(CONFIG_NAME),
            serde_json::to_string_pretty(payload).unwrap() + "\n",
        )
        .unwrap();
        let before = snapshot(&stack);
        for cmd_args in [
            vec!["list", "--stack", &stack_s],
            vec!["claim", "alpha", "--stack", &stack_s],
            vec!["doctor", "--stack", &stack_s],
        ] {
            let (code, _, err) = run_cli(&cmd_args);
            assert_eq!(code, 2, "payload {idx} on {cmd_args:?}: {err}");
            assert!(err.contains("corrupt"), "payload {idx}: {err}");
        }
        assert_eq!(snapshot(&stack), before, "payload {idx} mutated disk");
    }
}

#[test]
fn single_and_stray_braces_in_env_template_refuse_at_init_and_load_config() {
    let tmp = temp_dir("single-stray-braces");
    let brace_cases = [
        ("single-port-0", "{port_0}"),
        ("lone-open", "{"),
        ("lone-close", "}"),
        ("trailing-extra-close", "{{port_0}}}"),
        ("leading-single-before-valid", "{foo}{{port_0}}"),
        ("triple-open-close", "{{{port_0}}}"),
        ("trailing-single-open", "{{port_0}}{"),
    ];

    for (label, tpl_val) in brace_cases {
        let tpl = tmp.join(format!("brace-{label}.tpl"));
        fs::write(&tpl, format!("APP_URL={tpl_val}\n")).unwrap();
        let stack = tmp.join(format!("init-{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--env-template",
            &tpl.to_string_lossy(),
        ]);
        assert_eq!(
            code, 1,
            "init --env-template must refuse brace case {label} ({tpl_val:?}), stderr: {err}"
        );
        assert!(!stack.exists(), "{label} left stack residue");
    }

    let cfg_stack = tmp.join("cfg_stack");
    let cfg_stack_s = cfg_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &cfg_stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");

    for (label, tpl_val) in brace_cases {
        let payload = serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "APP_URL", "template": tpl_val}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        });
        fs::write(
            cfg_stack.join(CONFIG_NAME),
            serde_json::to_string_pretty(&payload).unwrap() + "\n",
        )
        .unwrap();
        let before = snapshot(&cfg_stack);
        let (code, _, err) = run_cli(&["list", "--stack", &cfg_stack_s]);
        assert_eq!(
            code, 2,
            "load_config must refuse brace case {label} ({tpl_val:?}) at exit 2: {err}"
        );
        assert!(err.contains("corrupt"), "{label}: {err}");
        assert_eq!(snapshot(&cfg_stack), before);
    }
}

#[test]
fn ascii_control_characters_in_env_template_refuse_at_init_and_load_config() {
    let tmp = temp_dir("ctrl-chars-env-template");
    let ctrl_cases: &[(&str, &[u8])] = &[
        ("nul", b"APP_VAL=ok\0tail\n"),
        ("bel-0x07", b"APP_VAL=ok\x07tail\n"),
        ("tab-0x09", b"APP_VAL=ok\ttail\n"),
        ("esc-0x1b", b"APP_VAL=ok\x1btail\n"),
        ("del-0x7f", b"APP_VAL=ok\x7ftail\n"),
    ];

    for (label, raw) in ctrl_cases {
        let tpl = tmp.join(format!("ctrl-{label}.tpl"));
        fs::write(&tpl, raw).unwrap();
        let stack = tmp.join(format!("init-ctrl-{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--env-template",
            &tpl.to_string_lossy(),
        ]);
        assert_eq!(
            code, 1,
            "init --env-template must refuse control byte {label}, stderr: {err}"
        );
        assert!(!stack.exists());
    }

    let cfg_stack = tmp.join("cfg_ctrl_stack");
    let cfg_stack_s = cfg_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &cfg_stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");

    for (label, tpl_str) in [
        ("nul", "ok\u{0000}tail"),
        ("bel", "ok\u{0007}tail"),
        ("tab", "ok\ttail"),
        ("esc", "ok\u{001b}tail"),
        ("del", "ok\u{007f}tail"),
    ] {
        let payload = serde_json::json!({
            "base_branch": "main",
            "env_template": [{"key": "APP_VAL", "template": tpl_str}],
            "probe_ports": false,
            "seed_files": [],
            "source_repo": null,
            "version": 1
        });
        fs::write(
            cfg_stack.join(CONFIG_NAME),
            serde_json::to_string_pretty(&payload).unwrap() + "\n",
        )
        .unwrap();
        let before = snapshot(&cfg_stack);
        let (code, _, err) = run_cli(&["list", "--stack", &cfg_stack_s]);
        assert_eq!(
            code, 2,
            "load_config must refuse control byte {label} at exit 2: {err}"
        );
        assert!(err.contains("corrupt"), "{label}: {err}");
        assert_eq!(snapshot(&cfg_stack), before);
    }
}

#[test]
fn worktreeinclude_and_config_refuse_case_and_prefix_collisions_and_branch_case_mismatches() {
    let tmp = temp_dir("case-prefix-collisions");
    let src = init_source_repo(&tmp);

    // Prepare on-disk files with mixed case (`Config/local.toml`, `Tracked.cfg`, `foo/bar`).
    fs::create_dir_all(src.join("Config")).unwrap();
    fs::write(src.join("Config/local.toml"), "a = 1\n").unwrap();
    fs::write(src.join("Tracked.cfg"), "tracked = 1\n").unwrap();
    fs::create_dir_all(src.join("foo")).unwrap();
    fs::write(src.join("foo/bar"), "nested\n").unwrap();
    fs::create_dir_all(src.join("Sub")).unwrap();
    fs::write(src.join("Sub/a.env"), "A=1\n").unwrap();
    if !src.join("sub").exists() {
        fs::create_dir_all(src.join("sub")).unwrap();
    }
    fs::write(src.join("sub/b.env"), "B=1\n").unwrap();
    fs::write(src.join("Foo.env"), "F=1\n").unwrap();
    if !src.join("foo.env").exists() {
        fs::write(src.join("foo.env"), "f=1\n").unwrap();
    }

    // 1. `.worktreeinclude` case collisions, prefix collisions, and on-disk case mismatches.
    let wti_cases = [
        ("case-file-collision", "Foo.env\nfoo.env\n"),
        ("case-dir-collision", "Sub/a.env\nsub/b.env\n"),
        ("prefix-collision", "foo\nfoo/bar\n"),
        ("case-prefix-collision", "FOO\nfoo/bar\n"),
        ("ondisk-dir-case-mismatch", "config/local.toml\n"),
        ("ondisk-leaf-case-mismatch", "tracked.cfg\n"),
    ];
    for (label, wti_body) in wti_cases {
        fs::write(src.join(WORKTREEINCLUDE_NAME), wti_body).unwrap();
        let stack = tmp.join(format!("wti-{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &stack.to_string_lossy(),
            "--from",
            &src.to_string_lossy(),
        ]);
        assert_eq!(
            code, 1,
            ".worktreeinclude case {label} must refuse at exit 1, stderr: {err}"
        );
        assert!(!stack.exists(), "{label} left stack residue");
    }

    // 2. Base-branch tree case mismatch on `init --from <src> --branch other`,
    //    tested against a PREEXISTING empty `stack` directory so rollback must
    //    leave `stack` empty (no `<stack>/repo` or `<stack>/seed` left behind).
    let branch_src_wrap = tmp.join("branch_case_wrap");
    fs::create_dir_all(&branch_src_wrap).unwrap();
    let branch_src = init_source_repo(&branch_src_wrap);
    git(&branch_src, &["checkout", "-q", "-b", "other"]);
    fs::create_dir_all(branch_src.join("AppDir")).unwrap();
    fs::write(branch_src.join("AppDir/committed.txt"), "on-other\n").unwrap();
    git(&branch_src, &["add", "AppDir/committed.txt"]);
    git(
        &branch_src,
        &["commit", "-q", "-m", "track AppDir on other"],
    );
    git(&branch_src, &["checkout", "-q", "main"]);
    fs::create_dir_all(branch_src.join("appdir")).unwrap();
    fs::write(branch_src.join("appdir/local.env"), "SEED=1\n").unwrap();
    fs::write(branch_src.join(WORKTREEINCLUDE_NAME), "appdir/local.env\n").unwrap();

    let pre_stack = tmp.join("preexisting_empty_stack");
    fs::create_dir_all(&pre_stack).unwrap();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &pre_stack.to_string_lossy(),
        "--from",
        &branch_src.to_string_lossy(),
        "--branch",
        "other",
    ]);
    assert_eq!(
        code, 1,
        "base-branch tree case mismatch (AppDir vs appdir) must refuse at exit 1, stderr: {err}"
    );
    assert!(
        pre_stack.is_dir(),
        "preexisting stack dir itself must remain"
    );
    assert_eq!(
        fs::read_dir(&pre_stack).unwrap().count(),
        0,
        "preexisting stack dir must be completely empty after rollback"
    );

    // 3. `load_config` (`switchyard-config.json`) case and prefix collisions refuse at exit 2!
    let cfg_stack = tmp.join("cfg_case_stack");
    let cfg_stack_s = cfg_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &cfg_stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");
    let valid_sha = "b".repeat(64);
    for (label, seeds) in [
        (
            "case-same-file",
            serde_json::json!([
                {"path": "Foo.env", "sha256": valid_sha},
                {"path": "foo.env", "sha256": valid_sha}
            ]),
        ),
        (
            "case-dir-component",
            serde_json::json!([
                {"path": "Sub/a.env", "sha256": valid_sha},
                {"path": "sub/b.env", "sha256": valid_sha}
            ]),
        ),
        (
            "exact-prefix",
            serde_json::json!([
                {"path": "foo", "sha256": valid_sha},
                {"path": "foo/bar", "sha256": valid_sha}
            ]),
        ),
        (
            "case-prefix-sorted",
            serde_json::json!([
                {"path": "FOO/bar", "sha256": valid_sha},
                {"path": "foo", "sha256": valid_sha}
            ]),
        ),
    ] {
        let payload = serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": seeds,
            "source_repo": null,
            "version": 1
        });
        fs::write(
            cfg_stack.join(CONFIG_NAME),
            serde_json::to_string_pretty(&payload).unwrap() + "\n",
        )
        .unwrap();
        let before = snapshot(&cfg_stack);
        let (code, _, err) = run_cli(&["list", "--stack", &cfg_stack_s]);
        assert_eq!(
            code, 2,
            "load_config seed collision {label} must exit 2: {err}"
        );
        assert!(err.contains("corrupt"), "{label}: {err}");
        assert_eq!(snapshot(&cfg_stack), before);
    }
}

#[test]
fn cli_scoping_help_and_non_utf8_env_template_pins() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = temp_dir("cli-scoping-help");
    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &stack_s]);
    assert_eq!(code, 0, "{err}");

    let (h_code, h_out, h_err) = run_cli(&["--help"]);
    assert_eq!(h_code, 0, "{h_err}");
    assert!(h_out.contains("--probe-ports"), "{h_out}");
    assert!(h_out.contains("--env-template"), "{h_out}");

    let bad_cli_calls: Vec<Vec<&str>> = vec![
        vec!["list", "--stack", &stack_s, "--probe-ports"],
        vec!["release", "alpha", "--stack", &stack_s, "--probe-ports"],
        vec!["doctor", "--stack", &stack_s, "--probe-ports"],
        vec!["init", "--stack", &stack_s, "--probe-ports=true"],
        vec![
            "claim",
            "alpha",
            "--stack",
            &stack_s,
            "--describe",
            "Run app",
            "--default-profile",
            "lean",
            "--judge-command",
            "/usr/bin/true",
            "--judge-arg",
            "--probe-ports",
        ],
        vec!["claim", "alpha", "--stack", &stack_s, "--env-template", "f"],
        vec!["list", "--stack", &stack_s, "--env-template", "f"],
        vec![
            "release",
            "alpha",
            "--stack",
            &stack_s,
            "--env-template",
            "f",
        ],
        vec!["doctor", "--stack", &stack_s, "--env-template", "f"],
        vec!["init", "--stack", &stack_s, "--env-template"],
        vec![
            "init",
            "--stack",
            &stack_s,
            "--env-template",
            "--probe-ports",
        ],
    ];
    for args in bad_cli_calls {
        let (code, _, err) = run_cli(&args);
        assert_eq!(code, 2, "expected exit 2 for {args:?}, stderr: {err}");
    }

    // End-to-end CLI non-UTF-8 `--env-template` path when the OS filesystem permits 0xff filenames.
    let raw_tpl_path = PathBuf::from(OsString::from_vec({
        let mut v = tmp.as_os_str().as_encoded_bytes().to_vec();
        v.extend_from_slice(b"/tpl-\xff.env");
        v
    }));
    if fs::write(&raw_tpl_path, b"CUSTOM_PORT={{port_0}}\n").is_ok() {
        let raw_stack = tmp.join("raw_tpl_stack");
        let output = Command::new(env!("CARGO_BIN_EXE_switchyard"))
            .arg("init")
            .arg("--stack")
            .arg(&raw_stack)
            .arg("--env-template")
            .arg(&raw_tpl_path)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (c_code, _, c_err) =
            run_cli(&["claim", "alpha", "--stack", &raw_stack.to_string_lossy()]);
        assert_eq!(c_code, 0, "{c_err}");
        let env_text =
            fs::read_to_string(raw_stack.join(WORKTREES_NAME).join("alpha").join(ENV_NAME))
                .unwrap();
        assert!(env_text.contains("\nCUSTOM_PORT=20000\n"), "{env_text}");
    }
}

#[test]
fn env_template_64kib_boundary_unreadable_file_and_newline_worktree_path_refusal() {
    let tmp = temp_dir("env-bounds-and-newline");

    // 1. Exact 65,536-byte `--env-template` succeeds at exit 0 (65,537 already pinned at exit 1).
    let tail_line = "BOUNDARY_PORT={{port_0}}\n";
    let pad_len = 65_536 - tail_line.len() - 1;
    let mut exact_64k = Vec::with_capacity(65_536);
    exact_64k.push(b'#');
    exact_64k.extend(std::iter::repeat_n(b'x', pad_len - 1));
    exact_64k.push(b'\n');
    exact_64k.extend_from_slice(tail_line.as_bytes());
    assert_eq!(exact_64k.len(), 65_536);

    let tpl_64k = tmp.join("exact_64k.tpl");
    fs::write(&tpl_64k, &exact_64k).unwrap();
    let ok_stack = tmp.join("ok_64k_stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &ok_stack.to_string_lossy(),
        "--env-template",
        &tpl_64k.to_string_lossy(),
    ]);
    assert_eq!(
        code, 0,
        "exact 65,536-byte env template must succeed: {err}"
    );
    let (code, _, err) = run_cli(&["claim", "alpha", "--stack", &ok_stack.to_string_lossy()]);
    assert_eq!(code, 0, "{err}");
    assert!(
        fs::read_to_string(ok_stack.join("worktrees/alpha").join(ENV_NAME))
            .unwrap()
            .contains("\nBOUNDARY_PORT=20000\n")
    );

    // 2. Unreadable `0o000` `--env-template` refuses `init` at exit 1 when non-root.
    if unsafe { libc::geteuid() } != 0 {
        let unreadable = tmp.join("unreadable.tpl");
        fs::write(&unreadable, "APP_PORT={{port_0}}\n").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let bad_stack = tmp.join("unreadable_stack");
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &bad_stack.to_string_lossy(),
            "--env-template",
            &unreadable.to_string_lossy(),
        ]);
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(code, 1, "unreadable env template must exit 1: {err}");
        assert!(!bad_stack.exists());
    }

    // 3. Worktree path containing `\n` refuses `.env.switchyard` creation and rolls back!
    let nl_parent = tmp.join("dir_with_\n_newline");
    if fs::create_dir_all(&nl_parent).is_ok() {
        let nl_stack = nl_parent.join("stack");
        let nl_stack_s = nl_stack.to_string_lossy().into_owned();
        let (code, _, err) = run_cli(&["init", "--stack", &nl_stack_s]);
        assert_eq!(code, 0, "{err}");
        let (c_code, _, c_err) = run_cli(&["claim", "alpha", "--stack", &nl_stack_s]);
        assert_eq!(c_code, 1, "newline in worktree path must refuse claim");
        assert!(
            c_err.contains("newline or control byte"),
            "unexpected stderr: {c_err}"
        );
        assert!(!nl_stack.join("worktrees/alpha").exists());
        assert_eq!(
            git_code(
                &nl_stack.join(REPO_NAME),
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    "refs/heads/switchyard/alpha"
                ]
            ),
            1,
            "failed claim must roll back refs/heads/switchyard/alpha"
        );
        assert!(load_registry(&nl_stack).unwrap().claims.is_empty());
    }
}

#[test]
fn symlink_or_dir_replacement_with_identical_sha256_refuses_in_seed_store_safe_release_and_doctor()
{
    let tmp = temp_dir("symlink-identical-sha256");
    let src = init_source_repo(&tmp);
    fs::write(src.join(".gitignore"), ".env.local\n").unwrap();
    git(&src, &["add", ".gitignore"]);
    git(&src, &["commit", "-q", "-m", "ignore .env.local"]);

    let payload = "IDENTICAL_SECRET_PAYLOAD=1\n";
    fs::write(src.join(".env.local"), payload).unwrap();
    fs::write(src.join(WORKTREEINCLUDE_NAME), ".env.local\n").unwrap();

    let outside_identical = tmp.join("outside_identical.txt");
    fs::write(&outside_identical, payload).unwrap();

    let stack = tmp.join("stack");
    let stack_s = stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack_s,
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "{err}");

    for agent in ["alpha", "orphan-sym", "orphan-dir"] {
        let (code, _, err) = run_cli(&["claim", agent, "--stack", &stack_s]);
        assert_eq!(code, 0, "{err}");
    }

    // 1. Replace `.env.local` in `alpha` worktree with a symlink to `outside_identical` (same SHA-256!):
    let alpha_env_local = stack.join(WORKTREES_NAME).join("alpha").join(".env.local");
    fs::remove_file(&alpha_env_local).unwrap();
    symlink(&outside_identical, &alpha_env_local).unwrap();
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 1,
        "symlink replacement with identical SHA-256 must refuse release --safe: {err}"
    );

    // Replace `.env.local` in `alpha` worktree with a directory:
    fs::remove_file(&alpha_env_local).unwrap();
    fs::create_dir(&alpha_env_local).unwrap();
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_s, "--safe"]);
    assert_eq!(
        code, 1,
        "directory replacement of gitignored seed must refuse release --safe: {err}"
    );
    fs::remove_dir(&alpha_env_local).unwrap();
    fs::write(&alpha_env_local, payload).unwrap();
    let (code, _, err) = run_cli(&["release", "alpha", "--stack", &stack_s, "--safe"]);
    assert_eq!(code, 0, "{err}");

    // 2. `doctor --fix` preserves both symlink-replaced (identical SHA-256) and dir-replaced orphans!
    let orphan_sym_leaf = stack
        .join(WORKTREES_NAME)
        .join("orphan-sym")
        .join(".env.local");
    fs::remove_file(&orphan_sym_leaf).unwrap();
    symlink(&outside_identical, &orphan_sym_leaf).unwrap();

    let orphan_dir_leaf = stack
        .join(WORKTREES_NAME)
        .join("orphan-dir")
        .join(".env.local");
    fs::remove_file(&orphan_dir_leaf).unwrap();
    fs::create_dir(&orphan_dir_leaf).unwrap();

    let reg_path = stack.join(REGISTRY_NAME);
    let mut reg_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&reg_path).unwrap()).unwrap();
    reg_json["claims"] = serde_json::json!({});
    fs::write(
        &reg_path,
        serde_json::to_string_pretty(&reg_json).unwrap() + "\n",
    )
    .unwrap();

    let report = switchyard::doctor(&stack, true).unwrap();
    assert!(!report.ok);
    let by_target: BTreeMap<String, (&str, bool)> = report
        .issues
        .iter()
        .filter(|i| i.target.starts_with("worktrees/"))
        .map(|i| (i.target.clone(), (i.kind.as_str(), i.fixed)))
        .collect();
    assert_eq!(
        by_target.get("worktrees/orphan-sym"),
        Some(&("dirty_orphan_worktree", false))
    );
    assert_eq!(
        by_target.get("worktrees/orphan-dir"),
        Some(&("dirty_orphan_worktree", false))
    );
    assert!(stack.join(WORKTREES_NAME).join("orphan-sym").is_dir());
    assert!(stack.join(WORKTREES_NAME).join("orphan-dir").is_dir());

    // 3. Replace `<stack>/seed/.env.local` itself with a symlink to `outside_identical` (same SHA-256!):
    let seed_store_leaf = stack.join(SEED_DIR_NAME).join(".env.local");
    fs::remove_file(&seed_store_leaf).unwrap();
    symlink(&outside_identical, &seed_store_leaf).unwrap();
    let (code, _, err) = run_cli(&["claim", "gamma", "--stack", &stack_s]);
    assert_eq!(
        code, 1,
        "symlink inside <stack>/seed must refuse claim even with identical SHA-256: {err}"
    );
    assert!(err.contains("symlink"), "{err}");
    assert!(!stack.join(WORKTREES_NAME).join("gamma").exists());
    assert_eq!(
        git_code(
            &stack.join(REPO_NAME),
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/switchyard/gamma"
            ]
        ),
        1
    );
}

#[test]
fn worktreeinclude_and_seed_files_exact_byte_and_count_boundaries() {
    let tmp = temp_dir("wti-seed-boundaries");
    let src = init_source_repo(&tmp);
    fs::write(src.join("ok.env"), "OK=1\n").unwrap();

    // 1. Invalid UTF-8 `.worktreeinclude` -> exit 1!
    fs::write(src.join(WORKTREEINCLUDE_NAME), b"ok.env\n\xff\n").unwrap();
    let s_utf8 = tmp.join("s_utf8");
    let (code, _, _) = run_cli(&[
        "init",
        "--stack",
        &s_utf8.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1);
    assert!(!s_utf8.exists());

    // 2. Exact 65,536-byte `.worktreeinclude` succeeds (exit 0), 65,537-byte fails (exit 1)!
    let entry = "ok.env\n";
    let pad_len = 65_536 - entry.len() - 1;
    let mut wti_64k = Vec::with_capacity(65_536);
    wti_64k.push(b'#');
    wti_64k.extend(std::iter::repeat_n(b'p', pad_len - 1));
    wti_64k.push(b'\n');
    wti_64k.extend_from_slice(entry.as_bytes());
    assert_eq!(wti_64k.len(), 65_536);
    fs::write(src.join(WORKTREEINCLUDE_NAME), &wti_64k).unwrap();
    let s_64k_ok = tmp.join("s_64k_ok");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &s_64k_ok.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "65,536-byte .worktreeinclude must succeed: {err}");

    let mut wti_64k_plus_1 = wti_64k;
    wti_64k_plus_1.insert(1, b'p');
    assert_eq!(wti_64k_plus_1.len(), 65_537);
    fs::write(src.join(WORKTREEINCLUDE_NAME), &wti_64k_plus_1).unwrap();
    let s_64k_bad = tmp.join("s_64k_bad");
    let (code, _, _) = run_cli(&[
        "init",
        "--stack",
        &s_64k_bad.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(code, 1);
    assert!(!s_64k_bad.exists());

    // 3. Component length 250 vs 251 bytes and path length 512 vs 513 bytes!
    let comp_250 = "c".repeat(250);
    let comp_251 = "c".repeat(251);
    fs::write(src.join(&comp_250), "250\n").unwrap();
    fs::write(src.join(&comp_251), "251\n").unwrap();

    // 200 + 1 + 200 + 1 + 110 = 512 bytes exact; +1 = 513 bytes!
    let d1 = "d".repeat(200);
    let d2 = "e".repeat(200);
    let f110 = "f".repeat(110);
    let f111 = "f".repeat(111);
    let path_512 = format!("{d1}/{d2}/{f110}");
    let path_513 = format!("{d1}/{d2}/{f111}");
    assert_eq!(path_512.len(), 512);
    assert_eq!(path_513.len(), 513);
    fs::create_dir_all(src.join(&d1).join(&d2)).unwrap();
    fs::write(src.join(&path_512), "512\n").unwrap();
    fs::write(src.join(&path_513), "513\n").unwrap();

    fs::write(
        src.join(WORKTREEINCLUDE_NAME),
        format!("{comp_250}\n{path_512}\n"),
    )
    .unwrap();
    let s_bounds_ok = tmp.join("s_bounds_ok");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &s_bounds_ok.to_string_lossy(),
        "--from",
        &src.to_string_lossy(),
    ]);
    assert_eq!(
        code, 0,
        "250-byte component and 512-byte path must succeed: {err}"
    );

    for (label, bad_line) in [("comp-251", &comp_251), ("path-513", &path_513)] {
        fs::write(src.join(WORKTREEINCLUDE_NAME), format!("{bad_line}\n")).unwrap();
        let bad_s = tmp.join(format!("s_{label}"));
        let (code, _, err) = run_cli(&[
            "init",
            "--stack",
            &bad_s.to_string_lossy(),
            "--from",
            &src.to_string_lossy(),
        ]);
        assert_eq!(code, 1, "{label} must fail init at exit 1: {err}");
        assert!(!bad_s.exists());
    }

    // 4. Seed count 256 (succeeds) vs 257 (refused at exit 1 in init and exit 2 in load_config)!
    let count_src_wrap = tmp.join("count_src_wrap");
    fs::create_dir_all(&count_src_wrap).unwrap();
    let count_src = init_source_repo(&count_src_wrap);
    fs::create_dir_all(count_src.join("many")).unwrap();
    let mut lines_256 = String::new();
    for idx in 0..257 {
        let rel = format!("many/s_{idx:03}.env");
        fs::write(count_src.join(&rel), format!("V={idx}\n")).unwrap();
        if idx < 256 {
            lines_256.push_str(&rel);
            lines_256.push('\n');
        }
    }
    fs::write(count_src.join(WORKTREEINCLUDE_NAME), &lines_256).unwrap();
    let s_256 = tmp.join("s_256");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &s_256.to_string_lossy(),
        "--from",
        &count_src.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "256 seed files must succeed: {err}");
    let (l_code, _, l_err) = run_cli(&["list", "--stack", &s_256.to_string_lossy()]);
    assert_eq!(l_code, 0, "{l_err}");

    let mut lines_257 = lines_256;
    lines_257.push_str("many/s_256.env\n");
    fs::write(count_src.join(WORKTREEINCLUDE_NAME), &lines_257).unwrap();
    let s_257 = tmp.join("s_257");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &s_257.to_string_lossy(),
        "--from",
        &count_src.to_string_lossy(),
    ]);
    assert_eq!(code, 1, "257 seed files must fail init at exit 1: {err}");
    assert!(!s_257.exists());

    // Also pin 251-byte component, 513-byte path, and 257 entries in `load_config` at exit 2!
    let cfg_stack = tmp.join("cfg_bounds_stack");
    let cfg_stack_s = cfg_stack.to_string_lossy().into_owned();
    let (code, _, err) = run_cli(&["init", "--stack", &cfg_stack_s, "--probe-ports"]);
    assert_eq!(code, 0, "{err}");
    let sha = "c".repeat(64);
    let seeds_257: Vec<serde_json::Value> = (0..257)
        .map(|i| serde_json::json!({"path": format!("many/s_{i:03}.env"), "sha256": sha}))
        .collect();
    for (label, bad_seeds) in [
        (
            "cfg-comp-251",
            serde_json::json!([{"path": comp_251, "sha256": sha}]),
        ),
        (
            "cfg-path-513",
            serde_json::json!([{"path": path_513, "sha256": sha}]),
        ),
        ("cfg-count-257", serde_json::Value::Array(seeds_257)),
    ] {
        let payload = serde_json::json!({
            "base_branch": "main",
            "env_template": null,
            "probe_ports": false,
            "seed_files": bad_seeds,
            "source_repo": null,
            "version": 1
        });
        fs::write(
            cfg_stack.join(CONFIG_NAME),
            serde_json::to_string_pretty(&payload).unwrap() + "\n",
        )
        .unwrap();
        let (code, _, err) = run_cli(&["list", "--stack", &cfg_stack_s]);
        assert_eq!(code, 2, "{label} must fail load_config at exit 2: {err}");
        assert!(err.contains("corrupt"), "{label}: {err}");
    }
}

#[test]
fn routed_waiting_no_consultation_fallback_default_refusal_and_prober_gap_skip() {
    let tmp = temp_dir("routed-edge-and-gap-skip");

    // 1. `claim_routed_waiting_with_options` when ALL free registry ranges have occupied host ports:
    //    settles on `default_profile` without consulting routing (`fallback == "no consultation"`,
    //    `refusal == "queued"`), spending only the 1 scoring consultation!
    let (base, l0) = bind_ipv4_in_pool(8, 0);
    let l1 = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, base + 4)).unwrap();
    let wstack = tmp.join("wstack");
    init_stack(&wstack, base as i128, 4, 2).unwrap();

    let judge = StubJudge::new("roomy", "6");
    let out = claim_routed_waiting_with_options(
        &wstack,
        "waiter",
        "Run app and worker.",
        Profile::Standard,
        None,
        Some(&judge),
        ClaimOptions { probe_ports: true },
    )
    .unwrap();
    match out {
        ClaimOutcome::Wait(wait) => {
            assert_eq!(wait.profile, "standard");
            assert_eq!(wait.score, Some(6));
        }
        ClaimOutcome::Claim(rec) => panic!("expected wait, got {rec:?}"),
    }
    let calls = judge.calls.borrow();
    assert_eq!(
        calls.len(),
        1,
        "routing consultation must be skipped when all host ports are occupied"
    );
    assert!(calls[0].contains("\"score\""));
    let rlines: Vec<serde_json::Value> = fs::read_to_string(wstack.join(ROUTING_LOG_NAME))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rlines.len(), 1);
    assert_eq!(rlines[0]["fallback"], "no consultation");
    assert_eq!(rlines[0]["verdict"], serde_json::Value::Null);
    assert_eq!(rlines[0]["allocated"], serde_json::Value::Null);
    assert_eq!(rlines[0]["refusal"], "queued");
    drop(l1);
    drop(l0);

    // 2. `claim_routed_with_options` Phase-3 fallback-to-default host-port refusal wording:
    //    Range 0 unbound (`lean` = 1 range is furnishable in Phase 1!), range 1 occupied on host
    //    so `standard` (2 ranges) has NO unbound span; judge abstains ("other"), landing on
    //    `default_profile = Standard`, which fails in Phase 3 naming `the default profile standard`!
    let (base2, l2_r1) = bind_ipv4_in_pool(8, 4);
    let dstack = tmp.join("dstack");
    init_stack(&dstack, base2 as i128, 4, 2).unwrap();
    let abstain_judge = StubJudge::new("other", "5");
    let err = claim_routed_with_options(
        &dstack,
        "fallback-agent",
        "Ambiguous workload.",
        Profile::Standard,
        Some(&abstain_judge),
        ClaimOptions { probe_ports: true },
    )
    .expect_err("default profile standard blocked on host port");
    assert_eq!(err.exit_code(), 1);
    assert_eq!(
        err.message(),
        "the default profile standard cannot be furnished, host loopback ports are occupied across every free candidate span in the port pool"
    );
    drop(l2_r1);

    // 3. `claim_profile_with_prober` end-to-end pin proving an isolated 1-slot free gap is
    //    never probed when `Profile::Standard` (`N = 2`) is requested!
    struct CountingProber {
        seen: std::cell::RefCell<Vec<u16>>,
    }
    impl PortProber for CountingProber {
        fn is_port_free(&self, port: u16) -> bool {
            self.seen.borrow_mut().push(port);
            true
        }
    }
    let gstack = tmp.join("gstack");
    init_stack(&gstack, 28_000, 4, 4).unwrap();
    // Claim range 0 (`temp0`) and range 1 (`holder`), then release `temp0` so slot 1
    // (28004..=28007) remains held and slot 0 (28000..=28003) stands as an isolated 1-slot gap!
    switchyard::claim(&gstack, "temp0").unwrap();
    switchyard::claim(&gstack, "holder").unwrap();
    switchyard::release(&gstack, "temp0").unwrap();

    let counting = CountingProber {
        seen: std::cell::RefCell::new(Vec::new()),
    };
    let rec = claim_profile_with_prober(
        &gstack,
        "wide",
        Profile::Standard,
        ClaimOptions { probe_ports: true },
        &counting,
    )
    .unwrap();
    assert_eq!((rec.port_start, rec.port_end), (28_008, 28_015));
    assert_eq!(
        *counting.seen.borrow(),
        (28_008..=28_015).collect::<Vec<u16>>()
    );
}
