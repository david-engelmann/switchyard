//! Tests for the wait queue slice obligations SY-17 to SY-24.
//!
//! Every fixture is synthetic. Stacks are built in temporary
//! directories by the library under test, standing waits are
//! staged by writing wait records into the registry document the
//! library itself validates on the next load, and transports are
//! test doubles standing behind the library seam, so no test in
//! this file performs a network call. The recorded fixture replay
//! pins stand in tests/queue_fixtures.rs and drive the fixtures
//! the recording window captured.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use switchyard::{
    claim, claim_profile, claim_profile_waiting, claim_routed_waiting, claim_waiting,
    init_stack, list_waits, load_registry, release, scoring_request, sha256_hex,
    ClaimOutcome, FixtureTransport, JudgeTransport, Profile, ReleaseOutcome, Wait,
    QUEUE_LOG_NAME, REGISTRY_NAME, ROUTING_LOG_NAME,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let number = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "switchyard-queue-test-{}-{number}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn stack_with(tag: &str, start: i128, range_size: i128, range_count: i128) -> PathBuf {
    let stack = temp_dir(tag).join("stack");
    init_stack(&stack, start, range_size, range_count).unwrap();
    stack
}

fn default_stack(tag: &str) -> PathBuf {
    stack_with(tag, 20000, 10, 16)
}

/// Fill every range of the default pool with plain claims, so no
/// draw of any size stands free.
fn fill_pool(stack: &Path) {
    for index in 0..16 {
        claim(stack, &format!("holder-{index:02}")).unwrap();
    }
}

fn read_queue_log(stack: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(stack.join(QUEUE_LOG_NAME)).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn read_routing_log(stack: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(stack.join(ROUTING_LOG_NAME)).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn queue_log_field_set(line: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = line.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

const QUEUE_LOG_FIELDS: [&str; 11] = [
    "agent",
    "at",
    "confidence",
    "fallback",
    "kind",
    "model",
    "ports",
    "profile",
    "request_bytes",
    "request_sha256",
    "score",
];

/// A standing wait in the recorded shape, staged directly into a
/// registry document. A scoreless wait names the no transport
/// class and a null model, and a scored wait carries its score
/// and confidence under the word none, as the writer writes them.
fn staged_wait(agent: &str, profile: &str, score: Option<i64>) -> serde_json::Value {
    match score {
        Some(level) => serde_json::json!({
            "agent": agent,
            "profile": profile,
            "recorded_at": "2026-11-02T00:00:00+00:00",
            "wait_for": null,
            "score": level,
            "confidence": 0.9,
            "model": "jev-1.13.0",
            "fallback": "none",
        }),
        None => serde_json::json!({
            "agent": agent,
            "profile": profile,
            "recorded_at": "2026-11-02T00:00:00+00:00",
            "wait_for": null,
            "score": null,
            "confidence": null,
            "model": null,
            "fallback": "no transport",
        }),
    }
}

/// Insert standing waits into the registry document of a stack,
/// in the order given, which becomes their arrival order.
fn inject_waits(stack: &Path, waits: &[serde_json::Value]) {
    let path = stack.join(REGISTRY_NAME);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut document: serde_json::Value = serde_json::from_str(&text).unwrap();
    document
        .as_object_mut()
        .unwrap()
        .insert("waits".to_string(), serde_json::Value::Array(waits.to_vec()));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&document).unwrap() + "\n",
    )
    .unwrap();
}

/// Rewrite the recorded_at of one standing wait in place.
fn rewrite_recorded_at(stack: &Path, agent: &str, stamp: &str) {
    let path = stack.join(REGISTRY_NAME);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut document: serde_json::Value = serde_json::from_str(&text).unwrap();
    for wait in document["waits"].as_array_mut().unwrap() {
        if wait["agent"] == agent {
            wait["recorded_at"] = stamp.into();
        }
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&document).unwrap() + "\n",
    )
    .unwrap();
}

fn snapshot_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                pending.push(path);
            } else {
                snapshot.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    snapshot
}

/// A score reply body in the wire shape the endpoint returns,
/// carrying a confidence field that disagrees with the
/// probabilities map on purpose, the way the measured wire shape
/// does, so every pin here also proves the carried field is never
/// read.
fn score_body(choice: &str, confidence: f64) -> String {
    let labels = [
        "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "other",
    ];
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
        "usage": {"input_tokens": 400, "output_tokens": 30},
    })
    .to_string()
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
        "usage": {"input_tokens": 400, "output_tokens": 30},
    })
    .to_string()
}

struct FixedTransport {
    body: String,
}

impl JudgeTransport for FixedTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        Ok(self.body.clone())
    }
}

struct CountingTransport {
    calls: RefCell<usize>,
    requests: RefCell<Vec<String>>,
    body: String,
}

impl CountingTransport {
    fn new(body: String) -> CountingTransport {
        CountingTransport {
            calls: RefCell::new(0),
            requests: RefCell::new(Vec::new()),
            body,
        }
    }

    fn calls(&self) -> usize {
        *self.calls.borrow()
    }
}

impl JudgeTransport for CountingTransport {
    fn consult(&self, request: &str) -> Result<String, String> {
        *self.calls.borrow_mut() += 1;
        self.requests.borrow_mut().push(request.to_string());
        Ok(self.body.clone())
    }
}

/// A transport that fails in the transport shape.
struct BrokenTransport;

impl JudgeTransport for BrokenTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        Err("the judge is down in this test".to_string())
    }
}

fn waited(outcome: ClaimOutcome) -> Wait {
    match outcome {
        ClaimOutcome::Wait(wait) => wait,
        ClaimOutcome::Claim(record) => {
            panic!("expected a wait, got a claim for {}", record.agent)
        }
    }
}

fn claimed(outcome: ClaimOutcome) -> switchyard::Record {
    match outcome {
        ClaimOutcome::Claim(record) => record,
        ClaimOutcome::Wait(wait) => panic!("expected a claim, got a wait for {}", wait.agent),
    }
}

// SY-17, the waiting surface

#[test]
fn a_waiting_plain_claim_waits_on_a_full_pool_and_creates_nothing_but_its_record() {
    let stack = default_stack("surface-full");
    fill_pool(&stack);
    let outcome = claim_waiting(&stack, "waiter", None, None).unwrap();
    let wait = waited(outcome);
    assert_eq!(wait.agent, "waiter");
    assert_eq!(wait.profile, "plain");
    assert_eq!(wait.score, None);
    assert_eq!(wait.confidence, None);
    assert_eq!(wait.model, None);
    assert_eq!(wait.fallback, "no transport");
    assert_eq!(wait.wait_for, None);
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 16);
    assert_eq!(data.waits.len(), 1);
    assert!(!stack.join("worktrees/waiter").exists());
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "queued");
    assert_eq!(lines[0]["agent"], "waiter");
}

#[test]
fn a_waiting_claim_allocates_exactly_as_without_the_option_where_its_draw_stands_free() {
    let stack = default_stack("surface-free");
    let record = claimed(claim_waiting(&stack, "alpha", None, None).unwrap());
    assert_eq!((record.port_start, record.port_end), (20000, 20009));
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.is_empty());
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    assert!(!text.contains("\"waits\""), "{text}");
    assert!(!stack.join(QUEUE_LOG_NAME).exists());
}

#[test]
fn waiting_profiled_claims_wait_with_their_named_profiles() {
    let stack = stack_with("surface-profiles", 20000, 10, 3);
    claim(&stack, "one").unwrap();
    claim(&stack, "two").unwrap();
    let roomy = waited(
        claim_profile_waiting(&stack, "big", Profile::Roomy, None, None).unwrap(),
    );
    assert_eq!(roomy.profile, "roomy");
    let standard = waited(
        claim_profile_waiting(&stack, "mid", Profile::Standard, None, None).unwrap(),
    );
    assert_eq!(standard.profile, "standard");
    let lean = claimed(
        claim_profile_waiting(&stack, "small", Profile::Lean, None, None).unwrap(),
    );
    assert_eq!((lean.port_start, lean.port_end), (20020, 20029));
}

#[test]
fn a_waiting_routed_claim_waits_on_the_profile_its_consultation_settled() {
    let stack = default_stack("surface-routed");
    for index in 0..15 {
        claim(&stack, &format!("holder-{index:02}")).unwrap();
    }
    let transport = FixedTransport {
        body: verdict_body("standard", 0.9),
    };
    let outcome = claim_routed_waiting(
        &stack,
        "routed",
        "Run the web app and its API while I fix the checkout flow.",
        Profile::Lean,
        Some(&transport),
        None,
    )
    .unwrap();
    let wait = waited(outcome);
    assert_eq!(wait.profile, "standard");
    let lines = read_routing_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["verdict"], "standard");
    assert_eq!(lines[0]["confidence"], 0.9);
    assert_eq!(lines[0]["allocated"], serde_json::Value::Null);
    assert_eq!(lines[0]["refusal"], "queued");
}

#[test]
fn a_wait_bound_that_is_not_positive_is_refused_before_anything_is_read() {
    let stack = temp_dir("surface-bound").join("stack");
    for bound in [0, -5] {
        let error = claim_waiting(&stack, "alpha", Some(bound), None)
            .expect_err("a bound that is not positive is refused outright");
        assert!(
            error.message().contains("positive number of seconds"),
            "{}",
            error.message()
        );
    }
    assert!(!stack.exists());
    let stack = default_stack("surface-bound-live");
    let before = std::fs::read(stack.join(REGISTRY_NAME)).unwrap();
    let error = claim_waiting(&stack, "alpha", Some(0), None)
        .expect_err("a bound of zero is refused on a live stack too");
    assert!(error.message().contains("positive"));
    assert_eq!(std::fs::read(stack.join(REGISTRY_NAME)).unwrap(), before);
    assert!(!stack.join(QUEUE_LOG_NAME).exists());
    fill_pool(&stack);
    let wait = waited(claim_waiting(&stack, "alpha", Some(90), None).unwrap());
    assert_eq!(wait.wait_for, Some(90));
}

#[test]
fn the_sixty_fifth_wait_refuses_with_a_message_naming_the_bound() {
    let stack = default_stack("surface-bound64");
    fill_pool(&stack);
    for index in 0..64 {
        let outcome = claim_waiting(&stack, &format!("waiter-{index:02}"), None, None).unwrap();
        waited(outcome);
    }
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.waits.len(), 64);
    let error = claim_waiting(&stack, "one-too-many", None, None)
        .expect_err("the sixty fifth wait refuses");
    assert!(error.message().contains("64"), "{}", error.message());
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 64);
}

// SY-18, the wait record and the registry growth

#[test]
fn a_recorded_wait_reads_back_field_by_field() {
    let stack = default_stack("record-fields");
    fill_pool(&stack);
    let request = scoring_request("plain", 1, &[], &[], Some(120), None);
    let transport = FixtureTransport::from_pairs([(
        sha256_hex(request.as_bytes()),
        score_body("7", 0.9),
    )]);
    let wait = waited(
        claim_waiting(&stack, "scored", Some(120), Some(&transport)).unwrap(),
    );
    assert_eq!(wait.agent, "scored");
    assert_eq!(wait.profile, "plain");
    assert_eq!(wait.wait_for, Some(120));
    assert_eq!(wait.score, Some(7));
    assert_eq!(wait.confidence, Some(0.9));
    assert_eq!(wait.model.as_deref(), Some("jev-1.13.0"));
    assert_eq!(wait.fallback, "none");
    assert!(switchyard::is_valid_timestamp(&wait.recorded_at));
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.waits, vec![wait]);
}

#[test]
fn the_registry_carries_the_waits_key_exactly_while_a_wait_stands() {
    let stack = default_stack("record-key");
    fill_pool(&stack);
    claim_waiting(&stack, "waiter", None, None).unwrap();
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    assert!(text.contains("\"waits\""), "{text}");
    assert!(text.contains("\"version\": 1"), "{text}");
    let outcome = release(&stack, "waiter").unwrap();
    assert!(matches!(outcome, ReleaseOutcome::Wait(_)));
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    assert!(!text.contains("\"waits\""), "{text}");
}

#[test]
fn wait_record_corruptions_refuse_as_corrupt_and_change_nothing() {
    let good = staged_wait("waiter", "standard", Some(7));
    let mut corruptions: Vec<(&str, serde_json::Value)> = Vec::new();
    let mut extra = good.clone();
    extra
        .as_object_mut()
        .unwrap()
        .insert("priority".to_string(), serde_json::json!(1));
    corruptions.push(("an unrecorded field", extra));
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("model");
    corruptions.push(("a missing field", missing));
    let mut bad_agent = good.clone();
    bad_agent["agent"] = "Not A Name".into();
    corruptions.push(("an agent name SY-10 refuses", bad_agent));
    let mut bad_profile = good.clone();
    bad_profile["profile"] = "huge".into();
    corruptions.push(("a profile outside the four", bad_profile));
    let mut lone_score = good.clone();
    lone_score["confidence"] = serde_json::Value::Null;
    corruptions.push(("a score standing beside a null confidence", lone_score));
    let mut lone_confidence = good.clone();
    lone_confidence["score"] = serde_json::Value::Null;
    corruptions.push(("a confidence standing beside a null score", lone_confidence));
    let mut big_score = good.clone();
    big_score["score"] = serde_json::json!(11);
    corruptions.push(("a score outside one to ten", big_score));
    let mut big_confidence = good.clone();
    big_confidence["confidence"] = serde_json::json!(1.5);
    corruptions.push(("a confidence outside zero to one", big_confidence));
    let mut bad_fallback = good.clone();
    bad_fallback["fallback"] = "urgent".into();
    corruptions.push(("a fallback outside the vocabulary", bad_fallback));
    let mut scored_none = staged_wait("waiter", "standard", None);
    scored_none["fallback"] = "none".into();
    corruptions.push(("the word none beside no score", scored_none));
    for (name, entry) in corruptions {
        let stack = default_stack("record-corrupt");
        claim(&stack, "holder").unwrap();
        inject_waits(&stack, &[entry]);
        let before = snapshot_tree(&stack);
        let error = claim(&stack, "other")
            .expect_err("a corrupt wait record refuses every command");
        assert_eq!(error.exit_code(), 2, "{name}");
        assert!(
            error.message().contains("corrupt"),
            "{name}: {}",
            error.message()
        );
        let listed = list_waits(&stack).expect_err("list refuses a corrupt wait record");
        assert_eq!(listed.exit_code(), 2, "{name}");
        assert_eq!(snapshot_tree(&stack), before, "{name}");
    }
    let stack = default_stack("record-both");
    claim(&stack, "holder").unwrap();
    inject_waits(&stack, &[staged_wait("holder", "lean", None)]);
    let error = claim(&stack, "other")
        .expect_err("an agent in both the claims and the waits is corrupt");
    assert_eq!(error.exit_code(), 2);
    let stack = default_stack("record-twice");
    inject_waits(
        &stack,
        &[
            staged_wait("waiter", "lean", None),
            staged_wait("waiter", "roomy", None),
        ],
    );
    let error = claim(&stack, "other")
        .expect_err("one agent standing twice in the waits is corrupt");
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn a_preslice_registry_document_loads_with_its_queue_read_as_empty() {
    let stack = default_stack("record-preslice");
    claim(&stack, "holder").unwrap();
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.is_empty());
    assert!(list_waits(&stack).unwrap().is_empty());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/python_registry_after_claim.json");
    let dir = temp_dir("record-preslice-doc");
    let stack = dir.join("stack");
    init_stack(&stack, 20000, 10, 16).unwrap();
    std::fs::copy(&fixture, stack.join(REGISTRY_NAME)).unwrap();
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 1);
    assert!(data.waits.is_empty());
}

#[test]
fn a_no_waits_document_this_slice_writes_matches_the_landed_writer_shape() {
    let stack = default_stack("record-shape");
    claim_profile(&stack, "alpha", Profile::Standard).unwrap();
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    let stamp = "\"claimed_at\": \"";
    let start = text.find(stamp).unwrap() + stamp.len();
    let end = text[start..].find('"').unwrap() + start;
    let blanked = format!("{}STAMP{}", &text[..start], &text[end..]);
    let expected = serde_json::json!({
        "claims": {
            "alpha": {
                "agent": "alpha",
                "branch": "switchyard/alpha",
                "claimed_at": "STAMP",
                "database": "switchyard_alpha",
                "env_file": "worktrees/alpha/.env.switchyard",
                "port_end": 20019,
                "port_start": 20000,
                "worktree": "worktrees/alpha",
            }
        },
        "port_pool": {"range_count": 16, "range_size": 10, "start": 20000},
        "version": 1,
    });
    assert_eq!(
        blanked,
        serde_json::to_string_pretty(&expected).unwrap() + "\n"
    );
}

// SY-19, the waiting envelope

#[test]
fn a_counting_transport_stands_at_zero_behind_every_first_phase_refusal() {
    let body = score_body("5", 0.9);
    // An invalid name.
    let stack = default_stack("envelope-name");
    let transport = CountingTransport::new(body.clone());
    claim_waiting(&stack, "Not A Name", None, Some(&transport))
        .expect_err("an invalid name refuses");
    assert_eq!(transport.calls(), 0);
    // An uninitialised stack.
    let stack = temp_dir("envelope-uninit").join("stack");
    let transport = CountingTransport::new(body.clone());
    claim_waiting(&stack, "alpha", None, Some(&transport))
        .expect_err("an uninitialised stack refuses");
    assert_eq!(transport.calls(), 0);
    // A corrupt registry.
    let stack = default_stack("envelope-corrupt");
    std::fs::write(stack.join(REGISTRY_NAME), "not a registry").unwrap();
    let transport = CountingTransport::new(body.clone());
    claim_waiting(&stack, "alpha", None, Some(&transport))
        .expect_err("a corrupt registry refuses");
    assert_eq!(transport.calls(), 0);
    // A standing claim.
    let stack = default_stack("envelope-claim");
    claim(&stack, "alpha").unwrap();
    let transport = CountingTransport::new(body.clone());
    claim_waiting(&stack, "alpha", None, Some(&transport))
        .expect_err("a standing claim refuses as duplicate");
    assert_eq!(transport.calls(), 0);
    // A standing wait.
    let stack = default_stack("envelope-wait");
    fill_pool(&stack);
    claim_waiting(&stack, "alpha", None, None).unwrap();
    let transport = CountingTransport::new(body.clone());
    let error = claim_waiting(&stack, "alpha", None, Some(&transport))
        .expect_err("a standing wait refuses as duplicate");
    assert!(
        error.message().contains("already holds a claim"),
        "{}",
        error.message()
    );
    assert_eq!(transport.calls(), 0);
    // A full queue.
    let stack = default_stack("envelope-full");
    fill_pool(&stack);
    let fillers: Vec<serde_json::Value> = (0..64)
        .map(|index| staged_wait(&format!("filler-{index:02}"), "lean", None))
        .collect();
    inject_waits(&stack, &fillers);
    let transport = CountingTransport::new(body.clone());
    claim_waiting(&stack, "alpha", None, Some(&transport))
        .expect_err("a full queue refuses");
    assert_eq!(transport.calls(), 0);
    // A bound that is not positive.
    let stack = default_stack("envelope-bound");
    let transport = CountingTransport::new(body);
    claim_waiting(&stack, "alpha", Some(0), Some(&transport))
        .expect_err("a bound of zero refuses");
    assert_eq!(transport.calls(), 0);
}

/// A transport that fills the queue while a consultation of a
/// routed claim stands open, by driving waiting claims of its
/// own on the same stack with no transport standing behind them.
/// One filler takes the one free range and the rest wait, so the
/// queue stands full when the claim reaches its third phase.
struct FillQueueOnConsultTransport {
    stack: PathBuf,
    fired: RefCell<bool>,
}

impl JudgeTransport for FillQueueOnConsultTransport {
    fn consult(&self, request: &str) -> Result<String, String> {
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            for index in 0..65 {
                claim_waiting(&self.stack, &format!("filler-{index:02}"), None, None)
                    .map_err(|error| error.message().to_string())?;
            }
        }
        if request.contains("\"profile\"") {
            Ok(verdict_body("standard", 0.9))
        } else {
            Ok(score_body("5", 0.9))
        }
    }
}

#[test]
fn a_queue_filled_during_a_consultation_refuses_queue_full_and_records_it() {
    let stack = default_stack("envelope-filled");
    for index in 0..15 {
        claim(&stack, &format!("holder-{index:02}")).unwrap();
    }
    let transport = FillQueueOnConsultTransport {
        stack: stack.clone(),
        fired: RefCell::new(false),
    };
    let error = claim_routed_waiting(
        &stack,
        "routed",
        "Run the web app and its API while I fix the checkout flow.",
        Profile::Lean,
        Some(&transport),
        None,
    )
    .expect_err("a queue filled during the consultations refuses as queue full");
    assert!(
        error.message().contains("queue is full"),
        "{}",
        error.message()
    );
    let lines = read_routing_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["refusal"], "queue full");
    assert_eq!(lines[0]["verdict"], "standard");
    assert_eq!(lines[0]["allocated"], serde_json::Value::Null);
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.waits.len(), 64);
    assert!(data.waits.iter().all(|wait| wait.agent != "routed"));
}

#[test]
fn two_waiting_claims_for_one_draw_allocate_and_wait_in_arrival_order() {
    let stack = stack_with("envelope-arrival", 20000, 10, 2);
    claim(&stack, "holder").unwrap();
    let first = claimed(claim_waiting(&stack, "first", None, None).unwrap());
    assert_eq!((first.port_start, first.port_end), (20010, 20019));
    let second = waited(claim_waiting(&stack, "second", None, None).unwrap());
    assert_eq!(second.profile, "plain");
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 2);
    assert_eq!(data.waits.len(), 1);
    let mut spans: Vec<(i64, i64)> = data
        .claims
        .values()
        .map(|record| (record.port_start, record.port_end))
        .collect();
    spans.sort();
    assert!(spans[1].0 > spans[0].1, "{spans:?}");
}

/// A transport that takes the free ranges with plain claims
/// while the scoring consultation of a routed claim stands open,
/// so the third phase finds the settled draw gone.
struct TakeRangesOnScoreTransport {
    stack: PathBuf,
    fired: RefCell<bool>,
}

impl JudgeTransport for TakeRangesOnScoreTransport {
    fn consult(&self, request: &str) -> Result<String, String> {
        if request.contains("\"profile\"") {
            return Ok(verdict_body("standard", 0.9));
        }
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            claim(&self.stack, "racer-one").map_err(|error| error.message().to_string())?;
            claim(&self.stack, "racer-two").map_err(|error| error.message().to_string())?;
        }
        Ok(score_body("4", 0.8))
    }
}

#[test]
fn a_routed_waiting_claim_whose_settled_draw_is_taken_during_scoring_waits_with_its_score() {
    let stack = stack_with("envelope-raced", 20000, 10, 2);
    let transport = TakeRangesOnScoreTransport {
        stack: stack.clone(),
        fired: RefCell::new(false),
    };
    let outcome = claim_routed_waiting(
        &stack,
        "waiter",
        "Run the web app and its API while I fix the checkout flow.",
        Profile::Lean,
        Some(&transport),
        None,
    )
    .unwrap();
    let wait = waited(outcome);
    assert_eq!(wait.profile, "standard");
    assert_eq!(wait.score, Some(4));
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 2);
    assert_eq!(data.waits.len(), 1);
    let lines = read_routing_log(&stack);
    assert_eq!(lines[0]["refusal"], "queued");
    assert_eq!(lines[0]["verdict"], "standard");
}

// SY-20, the scoring seat and grammar

#[test]
fn one_reply_of_each_scoring_class_lands_with_its_class_recorded() {
    let mut cases: Vec<(&str, String, Option<i64>, &str)> = Vec::new();
    cases.push(("a score at the threshold", score_body("8", 0.5), Some(8), "none"));
    cases.push(("a score above the threshold", score_body("8", 0.9), Some(8), "none"));
    cases.push(("a score below the threshold", score_body("3", 0.49), None, "low confidence"));
    cases.push(("the no score label", score_body("other", 0.9), None, "abstention"));
    cases.push(("an empty body", String::new(), None, "silence"));
    cases.push((
        "a body with no score answer",
        "{\"model\": \"jev-1.13.0\", \"answers\": {}}".to_string(),
        None,
        "silence",
    ));
    let one = score_body("5", 0.9);
    cases.push(("a two document body", format!("{one}\n{one}"), None, "malformed"));
    let mut missing_label = serde_json::from_str::<serde_json::Value>(&score_body("5", 0.9)).unwrap();
    missing_label["answers"]["score"]["probabilities"]
        .as_object_mut()
        .unwrap()
        .remove("7");
    cases.push((
        "probabilities missing a label",
        missing_label.to_string(),
        None,
        "malformed",
    ));
    let mut out_of_range = serde_json::from_str::<serde_json::Value>(&score_body("5", 0.9)).unwrap();
    out_of_range["answers"]["score"]["probabilities"]["5"] = serde_json::json!(1.5);
    cases.push((
        "a probability out of range",
        out_of_range.to_string(),
        None,
        "malformed",
    ));
    let mut outside_label = serde_json::from_str::<serde_json::Value>(&score_body("5", 0.9)).unwrap();
    outside_label["answers"]["score"]["probabilities"]
        .as_object_mut()
        .unwrap()
        .insert("eleven".to_string(), serde_json::json!(0.0));
    cases.push((
        "a label outside the eleven",
        outside_label.to_string(),
        None,
        "malformed",
    ));
    for (name, body, score, fallback) in cases {
        let stack = default_stack("scoring-class");
        fill_pool(&stack);
        let transport = FixedTransport { body };
        let wait = waited(claim_waiting(&stack, "waiter", None, Some(&transport)).unwrap());
        assert_eq!(wait.score, score, "{name}");
        assert_eq!(wait.fallback, fallback, "{name}");
        if score.is_some() {
            assert!(wait.confidence.is_some(), "{name}");
            assert_eq!(wait.model.as_deref(), Some("jev-1.13.0"), "{name}");
        } else {
            assert_eq!(wait.confidence, None, "{name}");
        }
        let lines = read_queue_log(&stack);
        assert_eq!(lines[0]["fallback"], fallback, "{name}");
        assert_eq!(lines[0]["score"], serde_json::json!(score), "{name}");
    }
    let stack = default_stack("scoring-transport");
    fill_pool(&stack);
    let wait = waited(claim_waiting(&stack, "waiter", None, Some(&BrokenTransport)).unwrap());
    assert_eq!(wait.fallback, "transport");
    assert_eq!(wait.score, None);
    assert_eq!(wait.model.as_deref(), Some("jev-1.13.0"));
    let stack = default_stack("scoring-none");
    fill_pool(&stack);
    let wait = waited(claim_waiting(&stack, "waiter", None, None).unwrap());
    assert_eq!(wait.fallback, "no transport");
    assert_eq!(wait.model, None);
}

#[test]
fn the_two_document_body_lands_as_malformed_and_never_as_a_spliced_score() {
    let stack = default_stack("scoring-two-doc");
    fill_pool(&stack);
    let one = score_body("9", 0.95);
    let transport = FixedTransport {
        body: format!("{one}\n{one}"),
    };
    let wait = waited(claim_waiting(&stack, "waiter", None, Some(&transport)).unwrap());
    assert_eq!(wait.score, None);
    assert_eq!(wait.fallback, "malformed");
}

#[test]
fn the_scoring_request_document_is_pinned_byte_for_byte() {
    let standing = vec![
        Wait {
            agent: "first".to_string(),
            profile: "lean".to_string(),
            recorded_at: "2026-11-02T00:00:00+00:00".to_string(),
            wait_for: None,
            score: None,
            confidence: None,
            model: None,
            fallback: "no transport".to_string(),
        },
        Wait {
            agent: "second".to_string(),
            profile: "roomy".to_string(),
            recorded_at: "2026-11-02T00:00:01+00:00".to_string(),
            wait_for: Some(30),
            score: Some(8),
            confidence: Some(0.9),
            model: Some("jev-1.13.0".to_string()),
            fallback: "none".to_string(),
        },
    ];
    let request = scoring_request(
        "standard",
        2,
        &standing,
        &[1, 1],
        Some(90),
        Some("Run the indexer."),
    );
    let document: serde_json::Value = serde_json::from_str(&request).unwrap();
    assert_eq!(document["model"], "jev-1.13.0");
    let question = &document["questions"]["score"];
    assert_eq!(question["type"], "choice");
    let criteria = question["criteria"].as_object().unwrap();
    assert_eq!(criteria.len(), 11);
    assert!(criteria.contains_key("other"));
    let state = document["state"].as_str().unwrap();
    assert!(state.contains("settled on the standard profile, a draw of 2 ranges"), "{state}");
    assert!(state.contains("Waits already standing, 2."), "{state}");
    assert!(state.contains("Standing for a draw of 1 range, 1."), "{state}");
    assert!(state.contains("Standing for a draw of 2 ranges, 0."), "{state}");
    assert!(state.contains("Standing for a draw of 4 ranges, 1."), "{state}");
    assert!(state.contains("in ranges, are 1 and 1."), "{state}");
    assert!(state.contains("a wait bound of 90 seconds"), "{state}");
    assert!(state.contains("Run the indexer."), "{state}");
    let bare = scoring_request("plain", 1, &[], &[], None, None);
    let bare_document: serde_json::Value = serde_json::from_str(&bare).unwrap();
    let bare_state = bare_document["state"].as_str().unwrap();
    assert!(bare_state.contains("No run stands free in the pool."), "{bare_state}");
    assert!(bare_state.contains("carries no wait bound"), "{bare_state}");
    assert!(bare_state.contains("carries no description"), "{bare_state}");
    assert!(!bare_state.contains("first"), "{bare_state}");
}

// SY-21, the offer rule

#[test]
fn a_scored_wait_is_furnished_before_a_scoreless_wait_for_the_same_range() {
    let stack = stack_with("offer-scored", 20000, 10, 1);
    claim(&stack, "holder").unwrap();
    let scoreless = waited(claim_waiting(&stack, "early", None, None).unwrap());
    assert_eq!(scoreless.score, None);
    let transport = FixedTransport {
        body: score_body("9", 0.9),
    };
    let scored = waited(claim_waiting(&stack, "late", None, Some(&transport)).unwrap());
    assert_eq!(scored.score, Some(9));
    let outcome = release(&stack, "holder").unwrap();
    assert!(matches!(outcome, ReleaseOutcome::Claim(_)));
    let data = load_registry(&stack).unwrap();
    assert!(data.claims.contains_key("late"));
    assert_eq!(
        (data.claims["late"].port_start, data.claims["late"].port_end),
        (20000, 20009)
    );
    assert_eq!(data.waits.len(), 1);
    assert_eq!(data.waits[0].agent, "early");
    let lines = read_queue_log(&stack);
    let furnished: Vec<&serde_json::Value> =
        lines.iter().filter(|line| line["kind"] == "furnished").collect();
    assert_eq!(furnished.len(), 1);
    assert_eq!(furnished[0]["agent"], "late");
    assert_eq!(furnished[0]["ports"], serde_json::json!([20000, 20009]));
}

#[test]
fn a_skipped_roomy_wait_does_not_block_the_lean_wait_behind_it() {
    let stack = stack_with("offer-skip", 20000, 10, 2);
    claim(&stack, "holder-a").unwrap();
    claim(&stack, "holder-b").unwrap();
    let transport = FixedTransport {
        body: score_body("10", 0.95),
    };
    let roomy = waited(
        claim_profile_waiting(&stack, "big", Profile::Roomy, None, Some(&transport)).unwrap(),
    );
    assert_eq!(roomy.score, Some(10));
    waited(claim_waiting(&stack, "small", None, None).unwrap());
    release(&stack, "holder-a").unwrap();
    let data = load_registry(&stack).unwrap();
    assert!(data.claims.contains_key("small"));
    assert_eq!(
        (data.claims["small"].port_start, data.claims["small"].port_end),
        (20000, 20009)
    );
    assert_eq!(data.waits.len(), 1);
    assert_eq!(data.waits[0].agent, "big");
    assert_eq!(data.waits[0].profile, "roomy");
}

#[test]
fn tied_scores_break_by_arrival_and_scoreless_waits_follow_in_arrival_order() {
    let stack = stack_with("offer-ties", 20000, 10, 1);
    claim(&stack, "holder").unwrap();
    let transport = FixedTransport {
        body: score_body("7", 0.8),
    };
    waited(claim_waiting(&stack, "first", None, Some(&transport)).unwrap());
    waited(claim_waiting(&stack, "second", None, Some(&transport)).unwrap());
    waited(claim_waiting(&stack, "third", None, None).unwrap());
    release(&stack, "holder").unwrap();
    let data = load_registry(&stack).unwrap();
    assert!(data.claims.contains_key("first"));
    let standing: Vec<&str> = data.waits.iter().map(|wait| wait.agent.as_str()).collect();
    assert_eq!(standing, vec!["second", "third"]);
    let order: Vec<String> = list_waits(&stack)
        .unwrap()
        .iter()
        .map(|wait| wait.agent.clone())
        .collect();
    assert_eq!(order, vec!["second".to_string(), "third".to_string()]);
}

#[test]
fn one_release_serves_several_waits_in_offer_order_in_one_published_state() {
    let stack = stack_with("offer-many", 20000, 10, 4);
    let big = claim_profile(&stack, "big", Profile::Roomy).unwrap();
    assert_eq!((big.port_start, big.port_end), (20000, 20039));
    let five = FixedTransport {
        body: score_body("5", 0.8),
    };
    let nine = FixedTransport {
        body: score_body("9", 0.9),
    };
    waited(claim_waiting(&stack, "lean-five", None, Some(&five)).unwrap());
    waited(
        claim_profile_waiting(&stack, "standard-none", Profile::Standard, None, None).unwrap(),
    );
    waited(claim_waiting(&stack, "lean-nine", None, Some(&nine)).unwrap());
    release(&stack, "big").unwrap();
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.is_empty());
    assert_eq!(data.claims.len(), 3);
    let stamps: Vec<&str> = ["lean-nine", "lean-five", "standard-none"]
        .iter()
        .map(|agent| data.claims[*agent].claimed_at.as_str())
        .collect();
    assert_eq!(stamps[0], stamps[1]);
    assert_eq!(stamps[1], stamps[2]);
    assert_eq!(
        (data.claims["lean-nine"].port_start, data.claims["lean-nine"].port_end),
        (20000, 20009)
    );
    assert_eq!(
        (data.claims["lean-five"].port_start, data.claims["lean-five"].port_end),
        (20010, 20019)
    );
    assert_eq!(
        (
            data.claims["standard-none"].port_start,
            data.claims["standard-none"].port_end
        ),
        (20020, 20039)
    );
    let lines = read_queue_log(&stack);
    let furnished: Vec<&str> = lines
        .iter()
        .filter(|line| line["kind"] == "furnished")
        .map(|line| line["agent"].as_str().unwrap())
        .collect();
    assert_eq!(furnished, vec!["lean-nine", "lean-five", "standard-none"]);
}

#[test]
fn a_release_of_an_agent_holding_only_a_wait_makes_no_offer() {
    let stack = stack_with("offer-none", 20000, 10, 2);
    claim(&stack, "holder-a").unwrap();
    claim(&stack, "holder-b").unwrap();
    waited(
        claim_profile_waiting(&stack, "big", Profile::Roomy, None, None).unwrap(),
    );
    waited(claim_waiting(&stack, "small", None, None).unwrap());
    let outcome = release(&stack, "big").unwrap();
    assert!(matches!(outcome, ReleaseOutcome::Wait(_)));
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 2);
    assert_eq!(data.waits.len(), 1);
    assert_eq!(data.waits[0].agent, "small");
}

// SY-22, expiry and cancellation

fn stamp_seconds_ago(seconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let target = now - seconds;
    let days = target.div_euclid(86_400);
    let rest = target.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

#[test]
fn a_wait_stands_short_of_its_bound_and_is_swept_at_and_past_it() {
    for (tag, ago, stands) in [
        ("short", 3500, true),
        ("at", 3600, false),
        ("past", 7200, false),
    ] {
        let stack = default_stack(tag);
        fill_pool(&stack);
        waited(claim_waiting(&stack, "waiter", Some(3600), None).unwrap());
        let mut entry = staged_wait("waiter", "plain", None);
        entry["wait_for"] = serde_json::json!(3600);
        inject_waits(&stack, &[entry]);
        rewrite_recorded_at(&stack, "waiter", &stamp_seconds_ago(ago));
        // A publishing operation arrives, a plain claim that
        // itself waits, and the sweep runs at its head.
        waited(claim_waiting(&stack, "sweeper", None, None).unwrap());
        let data = load_registry(&stack).unwrap();
        let standing: Vec<&str> = data.waits.iter().map(|wait| wait.agent.as_str()).collect();
        if stands {
            assert!(standing.contains(&"waiter"), "{tag}: {standing:?}");
            assert_eq!(
                read_queue_log(&stack)
                    .iter()
                    .filter(|line| line["kind"] == "expired")
                    .count(),
                0,
                "{tag}"
            );
        } else {
            assert!(!standing.contains(&"waiter"), "{tag}: {standing:?}");
            let expired: Vec<&serde_json::Value> = read_queue_log(&stack)
                .iter()
                .filter(|line| line["kind"] == "expired")
                .collect::<Vec<_>>();
            assert_eq!(expired.len(), 1, "{tag}");
            assert_eq!(expired[0]["agent"], "waiter", "{tag}");
        }
    }
}

#[test]
fn a_stack_left_untouched_keeps_its_expired_wait_until_an_operation_arrives() {
    let stack = default_stack("expiry-untouched");
    fill_pool(&stack);
    waited(claim_waiting(&stack, "waiter", Some(60), None).unwrap());
    let mut entry = staged_wait("waiter", "plain", None);
    entry["wait_for"] = serde_json::json!(60);
    inject_waits(&stack, &[entry]);
    rewrite_recorded_at(&stack, "waiter", &stamp_seconds_ago(3600));
    let listed = list_waits(&stack).unwrap();
    assert_eq!(listed.len(), 1);
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    assert!(text.contains("\"waits\""), "{text}");
    claim(&stack, "latecomer").expect_err("the pool is full for a plain claim");
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    assert!(text.contains("\"waits\""), "{text}");
    waited(claim_waiting(&stack, "sweeper", None, None).unwrap());
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.iter().all(|wait| wait.agent != "waiter"));
}

#[test]
fn release_cancels_a_standing_wait_and_a_second_release_refuses() {
    let stack = default_stack("expiry-cancel");
    fill_pool(&stack);
    waited(claim_waiting(&stack, "waiter", None, None).unwrap());
    let outcome = release(&stack, "waiter").unwrap();
    match outcome {
        ReleaseOutcome::Wait(wait) => assert_eq!(wait.agent, "waiter"),
        ReleaseOutcome::Claim(_) => panic!("a wait cancellation returns the wait record"),
    }
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.is_empty());
    assert_eq!(data.claims.len(), 16);
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["kind"], "cancelled");
    assert_eq!(lines[1]["agent"], "waiter");
    assert_eq!(lines[1]["ports"], serde_json::Value::Null);
    let error = release(&stack, "waiter").expect_err("an agent holding nothing refuses");
    assert!(
        error.message().contains("agent waiter holds no claim in this stack"),
        "{}",
        error.message()
    );
}

#[test]
fn an_agent_whose_wait_was_swept_may_claim_again_in_the_same_operation() {
    let stack = stack_with("expiry-reclaim", 20000, 10, 2);
    claim(&stack, "holder").unwrap();
    let mut entry = staged_wait("returning", "plain", None);
    entry["wait_for"] = serde_json::json!(60);
    inject_waits(&stack, &[entry]);
    rewrite_recorded_at(&stack, "returning", &stamp_seconds_ago(3600));
    let record = claim(&stack, "returning").unwrap();
    assert_eq!((record.port_start, record.port_end), (20010, 20019));
    let data = load_registry(&stack).unwrap();
    assert!(data.waits.is_empty());
    assert!(data.claims.contains_key("returning"));
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "expired");
    assert_eq!(lines[0]["agent"], "returning");
    assert_eq!(lines[0]["request_sha256"], serde_json::Value::Null);
}

// SY-23 and SY-24, the queue log and the list

#[test]
fn the_queue_log_carries_its_field_set_through_each_leaving() {
    let stack = stack_with("log-fields", 20000, 10, 1);
    claim(&stack, "holder").unwrap();
    let transport = CountingTransport::new(score_body("6", 0.75));
    let wait = waited(claim_waiting(&stack, "waiter", None, Some(&transport)).unwrap());
    assert_eq!(wait.score, Some(6));
    let request = transport.requests.borrow()[0].clone();
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 1);
    let queued = &lines[0];
    assert_eq!(queue_log_field_set(queued), QUEUE_LOG_FIELDS);
    assert_eq!(queued["kind"], "queued");
    assert_eq!(queued["request_sha256"], sha256_hex(request.as_bytes()));
    assert_eq!(queued["request_bytes"], request.len());
    assert_eq!(queued["score"], 6);
    assert_eq!(queued["confidence"], 0.75);
    assert_eq!(queued["model"], "jev-1.13.0");
    assert_eq!(queued["fallback"], "none");
    assert_eq!(queued["ports"], serde_json::Value::Null);
    release(&stack, "holder").unwrap();
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 2);
    let furnished = &lines[1];
    assert_eq!(queue_log_field_set(furnished), QUEUE_LOG_FIELDS);
    assert_eq!(furnished["kind"], "furnished");
    assert_eq!(furnished["request_sha256"], queued["request_sha256"]);
    assert_eq!(furnished["request_bytes"], queued["request_bytes"]);
    assert_eq!(furnished["ports"], serde_json::json!([20000, 20009]));
    assert_eq!(furnished["score"], 6);
}

#[test]
fn no_command_outcome_changes_when_the_queue_log_is_removed() {
    let stack = stack_with("log-removed", 20000, 10, 1);
    claim(&stack, "holder").unwrap();
    waited(claim_waiting(&stack, "waiter", None, None).unwrap());
    std::fs::remove_file(stack.join(QUEUE_LOG_NAME)).unwrap();
    let listed = list_waits(&stack).unwrap();
    assert_eq!(listed.len(), 1);
    release(&stack, "holder").unwrap();
    let data = load_registry(&stack).unwrap();
    assert!(data.claims.contains_key("waiter"));
    assert!(data.waits.is_empty());
    let lines = read_queue_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["kind"], "furnished");
    assert_eq!(lines[0]["request_sha256"], serde_json::Value::Null);
    assert_eq!(lines[0]["ports"], serde_json::json!([20000, 20009]));
}

// The command line surface, driven against the binary.

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

fn cli_stack(tag: &str, range_count: i64) -> PathBuf {
    let stack = temp_dir(tag).join("stack");
    let (code, _, err) = run_cli(&[
        "init",
        "--stack",
        &stack.to_string_lossy(),
        "--range-count",
        &range_count.to_string(),
    ]);
    assert_eq!(code, 0, "{err}");
    stack
}

#[test]
fn the_wait_options_parse_in_their_valid_and_invalid_shapes() {
    let stack = cli_stack("cli-parse", 1);
    let stack_arg = stack.to_string_lossy().into_owned();
    let (code, _, _) = run_cli(&["claim", "--stack", &stack_arg, "holder"]);
    assert_eq!(code, 0);
    // A bound with no wait option refuses at exit 2.
    let (code, _, err) = run_cli(&[
        "claim", "--stack", &stack_arg, "waiter", "--wait-for", "30",
    ]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--wait-for"), "{err}");
    // A bound that is not a positive integer refuses at exit 2.
    for bound in ["0", "-3", "abc"] {
        let (code, _, err) = run_cli(&[
            "claim", "--stack", &stack_arg, "waiter", "--wait", "--wait-for", bound,
        ]);
        assert_eq!(code, 2, "{bound}: {err}");
    }
    // The queue option belongs to list alone.
    let (code, _, _) = run_cli(&["claim", "--stack", &stack_arg, "waiter", "--queue"]);
    assert_eq!(code, 2);
    let (code, _, _) = run_cli(&["release", "--stack", &stack_arg, "holder", "--wait"]);
    assert_eq!(code, 2);
    // A waiting claim on the full pool succeeds as a wait.
    let (code, out, err) = run_cli(&[
        "claim", "--stack", &stack_arg, "waiter", "--wait", "--wait-for", "30",
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("waiting waiter"), "{out}");
    // The plain list bytes carry only the claim table.
    let (code, out, _) = run_cli(&["list", "--stack", &stack_arg]);
    assert_eq!(code, 0);
    assert!(out.contains("holder"), "{out}");
    assert!(!out.contains("waiter"), "{out}");
    // The queue list prints the standing wait in offer order.
    let (code, out, _) = run_cli(&["list", "--stack", &stack_arg, "--queue"]);
    assert_eq!(code, 0);
    assert!(out.contains("waiter"), "{out}");
    assert!(out.contains("plain"), "{out}");
    assert!(out.contains("none"), "{out}");
    // Releasing the wait holder cancels the wait.
    let (code, out, err) = run_cli(&["release", "--stack", &stack_arg, "waiter"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("waiter"), "{out}");
    let (code, out, _) = run_cli(&["list", "--stack", &stack_arg, "--queue"]);
    assert_eq!(code, 0);
    assert!(!out.contains("waiter"), "{out}");
}

#[test]
fn the_queue_table_prints_a_mixed_queue_in_offer_order() {
    let stack = cli_stack("cli-queue", 1);
    let stack_arg = stack.to_string_lossy().into_owned();
    run_cli(&["claim", "--stack", &stack_arg, "holder"]);
    inject_waits(
        &stack,
        &[
            staged_wait("scoreless-first", "lean", None),
            staged_wait("scored-three", "lean", Some(3)),
            staged_wait("scored-nine", "roomy", Some(9)),
        ],
    );
    let (code, out, err) = run_cli(&["list", "--stack", &stack_arg, "--queue"]);
    assert_eq!(code, 0, "{err}");
    let nine = out.find("scored-nine").unwrap();
    let three = out.find("scored-three").unwrap();
    let none = out.find("scoreless-first").unwrap();
    assert!(nine < three && three < none, "{out}");
    let nine_line = out.lines().find(|line| line.contains("scored-nine")).unwrap();
    assert!(nine_line.contains('9'), "{nine_line}");
    let none_line = out
        .lines()
        .find(|line| line.contains("scoreless-first"))
        .unwrap();
    assert!(none_line.contains("none"), "{none_line}");
}

// The subprocess deadline of the consultation, driven against a
// judge command whose background descendant holds the reply pipe
// open past the deadline. The consultation must end inside the
// deadline all the same, landing the wait under the transport
// class, because the deadline bounds the whole consultation and
// not only the judge process itself.

#[test]
fn a_judge_descendant_holding_the_pipe_open_cannot_stretch_the_consultation() {
    use std::os::unix::fs::PermissionsExt;
    let stack = cli_stack("cli-deadline", 1);
    let stack_arg = stack.to_string_lossy().into_owned();
    let (code, _, _) = run_cli(&["claim", "--stack", &stack_arg, "holder"]);
    assert_eq!(code, 0);
    let judge = temp_dir("cli-deadline-judge").join("judge.sh");
    std::fs::write(
        &judge,
        "#!/bin/sh\nsleep 90 &\ncat > /dev/null\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&judge, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = std::time::Instant::now();
    let (code, out, err) = run_cli(&[
        "claim",
        "--stack",
        &stack_arg,
        "waiter",
        "--wait",
        "--judge-command",
        &judge.to_string_lossy(),
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("waiting waiter"), "{out}");
    assert!(
        elapsed < std::time::Duration::from_secs(45),
        "the consultation ran {elapsed:?}"
    );
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.waits.len(), 1);
    assert_eq!(data.waits[0].fallback, "transport");
    assert_eq!(data.waits[0].score, None);
}
