//! Tests for the routing slice obligations SY-11 to SY-16.
//!
//! Every fixture is synthetic. Stacks are built in temporary
//! directories by the library under test, and transports are test
//! doubles standing behind the library seam, so no test performs a
//! network call. The recorded fixture replay pins of the fallback
//! and log and request groups stand at the end of this file and
//! drive the fixtures the recording window captured.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use switchyard::{
    claim, claim_profile, claim_routed, init_stack, load_registry, profile_named, release,
    routing_request, sha256_hex, FixtureTransport, JudgeTransport, Profile, REGISTRY_NAME,
    REPO_NAME, ROUTING_LOG_NAME,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let number = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "switchyard-routing-test-{}-{number}-{tag}",
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

fn read_log(stack: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(stack.join(ROUTING_LOG_NAME)).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn log_field_set(line: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = line.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

const LOG_FIELDS: [&str; 10] = [
    "agent",
    "allocated",
    "at",
    "confidence",
    "description_bytes",
    "description_sha256",
    "fallback",
    "model",
    "refusal",
    "verdict",
];

/// A verdict reply body in the wire shape the endpoint returns,
/// carrying a confidence field that disagrees with the
/// probabilities map on purpose, the way the measured wire shape
/// does, so every pin here also proves the carried field is never
/// read.
fn verdict_body(choice: &str, probabilities: [(&str, f64); 4], carried: f64) -> String {
    let mut map = serde_json::Map::new();
    for (label, value) in probabilities {
        map.insert(label.to_string(), serde_json::json!(value));
    }
    serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            "profile": {
                "type": "choice",
                "choice": choice,
                "confidence": carried,
                "probabilities": map,
            }
        },
        "usage": {"input_tokens": 400, "output_tokens": 30},
    })
    .to_string()
}

fn probs(choice: &str, confidence: f64) -> [(&'static str, f64); 4] {
    let rest = (1.0 - confidence) / 3.0;
    ["lean", "standard", "roomy", "other"].map(|label| {
        if label == choice {
            (label, confidence)
        } else {
            (label, rest)
        }
    })
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
    body: String,
}

impl CountingTransport {
    fn new(body: String) -> CountingTransport {
        CountingTransport {
            calls: RefCell::new(0),
            body,
        }
    }

    fn calls(&self) -> usize {
        *self.calls.borrow()
    }
}

impl JudgeTransport for CountingTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        *self.calls.borrow_mut() += 1;
        Ok(self.body.clone())
    }
}

/// A transport that takes a claim of its own on the same stack
/// while the consultation stands open, the deterministic shape of
/// a verdict made unfurnishable between the phases of the
/// envelope.
struct ClaimOnConsultTransport {
    stack: PathBuf,
    agent: String,
    profile: Profile,
    body: String,
    fired: RefCell<bool>,
}

impl JudgeTransport for ClaimOnConsultTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            claim_profile(&self.stack, &self.agent, self.profile)
                .map_err(|error| error.message().to_string())?;
        }
        Ok(self.body.clone())
    }
}

/// A transport that takes a plain claim for the routed claim's
/// own agent while the consultation stands open, so the third
/// phase finds a standing claim for the agent under the lock
/// again.
struct ClaimSameAgentOnConsultTransport {
    stack: PathBuf,
    agent: String,
    body: String,
    fired: RefCell<bool>,
}

impl JudgeTransport for ClaimSameAgentOnConsultTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            claim(&self.stack, &self.agent).map_err(|error| error.message().to_string())?;
        }
        Ok(self.body.clone())
    }
}

/// A transport that corrupts the registry file while the
/// consultation stands open, so the registry read of the third
/// phase fails.
struct CorruptRegistryOnConsultTransport {
    stack: PathBuf,
    body: String,
    fired: RefCell<bool>,
}

impl JudgeTransport for CorruptRegistryOnConsultTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            std::fs::write(self.stack.join(REGISTRY_NAME), "not a registry")
                .map_err(|error| error.to_string())?;
        }
        Ok(self.body.clone())
    }
}

/// A transport that moves the stack repository's .git entry
/// aside while the consultation stands open, so the repository
/// verification of the third phase fails.
struct BreakRepoOnConsultTransport {
    stack: PathBuf,
    body: String,
    fired: RefCell<bool>,
}

impl JudgeTransport for BreakRepoOnConsultTransport {
    fn consult(&self, _request: &str) -> Result<String, String> {
        if !*self.fired.borrow() {
            *self.fired.borrow_mut() = true;
            let repo = self.stack.join(REPO_NAME);
            std::fs::rename(repo.join(".git"), repo.join(".git-moved"))
                .map_err(|error| error.to_string())?;
        }
        Ok(self.body.clone())
    }
}

const DESCRIPTION: &str = "Run the web app and its API while I fix the checkout flow.";

// SY-12, the declared profiles and their draws

#[test]
fn profiles_draw_their_spans_at_the_default_geometry() {
    let stack = default_stack("draw-default");
    let lean = claim_profile(&stack, "alpha", Profile::Lean).unwrap();
    assert_eq!((lean.port_start, lean.port_end), (20000, 20009));
    let standard = claim_profile(&stack, "beta", Profile::Standard).unwrap();
    assert_eq!((standard.port_start, standard.port_end), (20010, 20029));
    let roomy = claim_profile(&stack, "gamma", Profile::Roomy).unwrap();
    assert_eq!((roomy.port_start, roomy.port_end), (20030, 20069));
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 3);
    let mut spans: Vec<(i64, i64)> = data
        .claims
        .values()
        .map(|record| (record.port_start, record.port_end))
        .collect();
    spans.sort();
    for pair in spans.windows(2) {
        assert!(pair[1].0 > pair[0].1, "{spans:?}");
    }
    let record = &data.claims["beta"];
    assert_eq!(record.branch, "switchyard/beta");
    assert_eq!(record.database, "switchyard_beta");
    assert_eq!(record.worktree, "worktrees/beta");
}

#[test]
fn profiles_draw_whole_ranges_at_a_second_geometry() {
    let stack = stack_with("draw-second", 30000, 5, 8);
    let standard = claim_profile(&stack, "alpha", Profile::Standard).unwrap();
    assert_eq!((standard.port_start, standard.port_end), (30000, 30009));
    let roomy = claim_profile(&stack, "beta", Profile::Roomy).unwrap();
    assert_eq!((roomy.port_start, roomy.port_end), (30010, 30029));
    let lean = claim_profile(&stack, "gamma", Profile::Lean).unwrap();
    assert_eq!((lean.port_start, lean.port_end), (30030, 30034));
}

#[test]
fn a_fragmented_pool_refuses_a_draw_needing_a_run_and_a_plain_claim_still_succeeds() {
    let stack = stack_with("draw-fragmented", 20000, 10, 4);
    claim(&stack, "one").unwrap();
    claim(&stack, "two").unwrap();
    claim(&stack, "three").unwrap();
    release(&stack, "two").unwrap();
    let error = claim_profile(&stack, "four", Profile::Standard)
        .expect_err("a draw of two consecutive ranges cannot stand on a fragmented pool");
    assert!(
        error.message().contains("draw of 2 consecutive ranges"),
        "{}",
        error.message()
    );
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 2);
    let plain = claim(&stack, "four").unwrap();
    assert_eq!((plain.port_start, plain.port_end), (20010, 20019));
}

#[test]
fn registry_bytes_of_a_profiled_claim_match_the_landed_writer_shape() {
    let stack = default_stack("draw-bytes");
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

// SY-11, the routing surface at the library

#[test]
fn an_unknown_profile_name_is_refused() {
    let error = profile_named("huge").expect_err("an undeclared profile is refused");
    assert_eq!(error.exit_code(), 1);
    assert!(
        error.message().contains("unknown profile huge"),
        "{}",
        error.message()
    );
    assert!(
        error.message().contains("lean and standard and roomy"),
        "{}",
        error.message()
    );
    for name in ["lean", "standard", "roomy"] {
        assert_eq!(profile_named(name).unwrap().name(), name);
    }
}

#[test]
fn a_routed_claim_with_an_empty_description_is_refused_and_records_nothing() {
    let stack = default_stack("surface-empty");
    let transport = CountingTransport::new(verdict_body("lean", probs("lean", 0.9), 0.5));
    let error = claim_routed(&stack, "alpha", "   ", Profile::Lean, Some(&transport))
        .expect_err("an all whitespace description is a caller error");
    assert!(
        error.message().contains("the job description is empty"),
        "{}",
        error.message()
    );
    assert_eq!(transport.calls(), 0);
    assert!(!stack.join(ROUTING_LOG_NAME).exists());
    assert!(load_registry(&stack).unwrap().claims.is_empty());
}

#[test]
fn a_routed_claim_past_the_description_bound_is_refused_and_one_at_the_bound_is_not() {
    let stack = default_stack("surface-bound");
    let transport = CountingTransport::new(verdict_body("lean", probs("lean", 0.9), 0.5));
    let past = "x".repeat(4097);
    let error = claim_routed(&stack, "alpha", &past, Profile::Lean, Some(&transport))
        .expect_err("a description past the bound is refused before any consultation");
    assert!(error.message().contains("4096"), "{}", error.message());
    assert_eq!(transport.calls(), 0);
    let at_bound = "x".repeat(4096);
    let record = claim_routed(&stack, "alpha", &at_bound, Profile::Lean, None).unwrap();
    assert_eq!((record.port_start, record.port_end), (20000, 20009));
}

// SY-14 and SY-15, the no transport landing and its record

#[test]
fn a_routed_claim_with_no_transport_lands_on_its_default_and_records_it() {
    let stack = default_stack("no-transport");
    let record = claim_routed(&stack, "alpha", DESCRIPTION, Profile::Standard, None).unwrap();
    assert_eq!((record.port_start, record.port_end), (20000, 20019));
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(log_field_set(line), LOG_FIELDS);
    assert_eq!(line["agent"], "alpha");
    assert_eq!(line["model"], serde_json::Value::Null);
    assert_eq!(line["verdict"], serde_json::Value::Null);
    assert_eq!(line["confidence"], serde_json::Value::Null);
    assert_eq!(line["fallback"], "no transport");
    assert_eq!(line["allocated"], "standard");
    assert_eq!(line["refusal"], serde_json::Value::Null);
    assert_eq!(line["description_bytes"], DESCRIPTION.len());
    assert_eq!(
        line["description_sha256"],
        sha256_hex(DESCRIPTION.as_bytes())
    );
    assert!(line["at"].as_str().unwrap().contains('T'));
}

// SY-13, the envelope

#[test]
fn phase_one_refusals_consult_no_model() {
    // An invalid agent name.
    let stack = default_stack("envelope-name");
    let transport = CountingTransport::new(verdict_body("lean", probs("lean", 0.9), 0.5));
    let error = claim_routed(
        &stack,
        "Not A Name",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("an invalid name refuses in the first phase");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(transport.calls(), 0);

    // An uninitialised stack.
    let bare = temp_dir("envelope-bare").join("stack");
    let error = claim_routed(&bare, "alpha", DESCRIPTION, Profile::Lean, Some(&transport))
        .expect_err("an uninitialised stack refuses in the first phase");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(transport.calls(), 0);
    assert!(!bare.join(REGISTRY_NAME).exists());
    assert!(!bare.join(ROUTING_LOG_NAME).exists());

    // A corrupt registry.
    let stack = default_stack("envelope-corrupt");
    std::fs::write(stack.join(REGISTRY_NAME), "not a registry").unwrap();
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a corrupt registry refuses in the first phase");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(transport.calls(), 0);

    // A standing claim for the same agent.
    let stack = default_stack("envelope-duplicate");
    claim(&stack, "alpha").unwrap();
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a standing claim refuses in the first phase");
    assert!(
        error.message().contains("already holds a claim"),
        "{}",
        error.message()
    );
    assert_eq!(transport.calls(), 0);
}

#[test]
fn a_pool_that_can_furnish_nothing_refuses_before_any_consultation_and_records_it() {
    let stack = stack_with("envelope-exhausted", 20000, 10, 1);
    claim(&stack, "holder").unwrap();
    let transport = CountingTransport::new(verdict_body("lean", probs("lean", 0.9), 0.5));
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a pool holding no furnishable profile refuses in the first phase");
    assert!(
        error
            .message()
            .contains("no declared profile can be furnished"),
        "{}",
        error.message()
    );
    assert_eq!(transport.calls(), 0);
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(log_field_set(line), LOG_FIELDS);
    assert_eq!(line["agent"], "alpha");
    assert_eq!(line["model"], "jev-1.13.0");
    assert_eq!(line["verdict"], serde_json::Value::Null);
    assert_eq!(line["confidence"], serde_json::Value::Null);
    assert_eq!(line["fallback"], "no consultation");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "pool");
}

#[test]
fn two_routed_claims_racing_never_overlap() {
    let stack = stack_with("envelope-race", 20000, 10, 2);
    let body = verdict_body("lean", probs("lean", 0.9), 0.5);
    let handles: Vec<_> = ["racer-a", "racer-b"]
        .into_iter()
        .map(|agent| {
            let stack = stack.clone();
            let body = body.clone();
            std::thread::spawn(move || {
                let transport = FixedTransport { body };
                claim_routed(&stack, agent, DESCRIPTION, Profile::Roomy, Some(&transport))
            })
        })
        .collect();
    let mut spans: Vec<(i64, i64)> = handles
        .into_iter()
        .map(|handle| {
            let record = handle.join().unwrap().unwrap();
            (record.port_start, record.port_end)
        })
        .collect();
    spans.sort();
    assert_eq!(spans, vec![(20000, 20009), (20010, 20019)]);
    assert_eq!(read_log(&stack).len(), 2);
}

#[test]
fn a_verdict_made_unfurnishable_between_phases_refuses_and_records_it() {
    let stack = stack_with("envelope-verdict-starved", 20000, 10, 4);
    let transport = ClaimOnConsultTransport {
        stack: stack.clone(),
        agent: "squatter".to_string(),
        profile: Profile::Roomy,
        body: verdict_body("roomy", probs("roomy", 0.95), 0.5),
        fired: RefCell::new(false),
    };
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a verdict whose draw a racing claim took refuses in the third phase");
    assert!(
        error
            .message()
            .contains("profile roomy cannot be furnished"),
        "{}",
        error.message()
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["verdict"], "roomy");
    assert_eq!(line["confidence"], 0.95);
    assert_eq!(line["fallback"], "none");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "pool");
    let data = load_registry(&stack).unwrap();
    assert_eq!(data.claims.len(), 1);
    assert!(data.claims.contains_key("squatter"));
}

#[test]
fn a_default_made_unfurnishable_between_phases_refuses_as_a_pool_refusal() {
    let stack = stack_with("envelope-default-starved", 20000, 10, 2);
    let transport = ClaimOnConsultTransport {
        stack: stack.clone(),
        agent: "squatter".to_string(),
        profile: Profile::Standard,
        body: String::new(),
        fired: RefCell::new(false),
    };
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a default whose draw a racing claim took refuses as a pool refusal");
    assert!(
        error
            .message()
            .contains("the default profile lean cannot be furnished"),
        "{}",
        error.message()
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["verdict"], serde_json::Value::Null);
    assert_eq!(line["fallback"], "silence");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "pool");
}

#[test]
fn a_claim_taken_for_the_agent_during_the_consultation_refuses_as_a_duplicate_and_records_it() {
    let stack = default_stack("envelope-duplicate");
    let transport = ClaimSameAgentOnConsultTransport {
        stack: stack.clone(),
        agent: "alpha".to_string(),
        body: verdict_body("roomy", probs("roomy", 0.95), 0.5),
        fired: RefCell::new(false),
    };
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a standing claim found in the third phase refuses the routed claim");
    assert!(
        error.message().contains("already holds a claim"),
        "{}",
        error.message()
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(log_field_set(line), LOG_FIELDS);
    assert_eq!(line["verdict"], "roomy");
    assert_eq!(line["confidence"], 0.95);
    assert_eq!(line["fallback"], "none");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "duplicate");
}

#[test]
fn a_registry_corrupted_during_the_consultation_refuses_as_corrupt_and_records_it() {
    let stack = default_stack("envelope-corrupt");
    let transport = CorruptRegistryOnConsultTransport {
        stack: stack.clone(),
        body: verdict_body("roomy", probs("roomy", 0.95), 0.5),
        fired: RefCell::new(false),
    };
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a registry read failing in the third phase refuses the routed claim");
    assert!(
        error.message().contains("is corrupt"),
        "{}",
        error.message()
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(log_field_set(line), LOG_FIELDS);
    assert_eq!(line["verdict"], "roomy");
    assert_eq!(line["confidence"], 0.95);
    assert_eq!(line["fallback"], "none");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "corrupt");
}

#[test]
fn a_repository_broken_during_the_consultation_refuses_as_repo_and_records_it() {
    let stack = default_stack("envelope-repo");
    let transport = BreakRepoOnConsultTransport {
        stack: stack.clone(),
        body: verdict_body("roomy", probs("roomy", 0.95), 0.5),
        fired: RefCell::new(false),
    };
    let error = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .expect_err("a repository verification failing in the third phase refuses the routed claim");
    assert!(
        error.message().contains("has no .git entry of its own"),
        "{}",
        error.message()
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(log_field_set(line), LOG_FIELDS);
    assert_eq!(line["verdict"], "roomy");
    assert_eq!(line["confidence"], 0.95);
    assert_eq!(line["fallback"], "none");
    assert_eq!(line["allocated"], serde_json::Value::Null);
    assert_eq!(line["refusal"], "repo");
}

// SY-14, verdicts and fallbacks over constructed replies

struct FallbackOutcome {
    record: switchyard::Record,
    line: serde_json::Value,
}

fn run_fallback(body: Result<String, String>) -> FallbackOutcome {
    struct GivenTransport {
        body: Result<String, String>,
    }
    impl JudgeTransport for GivenTransport {
        fn consult(&self, _request: &str) -> Result<String, String> {
            self.body.clone()
        }
    }
    let stack = default_stack("fallback");
    let transport = GivenTransport { body };
    let record = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .unwrap();
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    FallbackOutcome {
        record,
        line: lines.into_iter().next().unwrap(),
    }
}

#[test]
fn a_verdict_at_the_threshold_stands_and_one_below_it_lands_on_the_default() {
    let outcome = run_fallback(Ok(verdict_body("standard", probs("standard", 0.5), 0.99)));
    assert_eq!(
        (outcome.record.port_start, outcome.record.port_end),
        (20000, 20019)
    );
    assert_eq!(outcome.line["verdict"], "standard");
    assert_eq!(outcome.line["confidence"], 0.5);
    assert_eq!(outcome.line["fallback"], "none");
    assert_eq!(outcome.line["allocated"], "standard");
    assert_eq!(outcome.line["model"], "jev-1.13.0");

    let outcome = run_fallback(Ok(verdict_body("standard", probs("standard", 0.49), 0.99)));
    assert_eq!(
        (outcome.record.port_start, outcome.record.port_end),
        (20000, 20009)
    );
    assert_eq!(outcome.line["verdict"], "standard");
    assert_eq!(outcome.line["confidence"], 0.49);
    assert_eq!(outcome.line["fallback"], "low confidence");
    assert_eq!(outcome.line["allocated"], "lean");
}

#[test]
fn the_recorded_confidence_is_the_probability_of_the_chosen_label() {
    // The carried confidence field of this body stands at 0.5
    // while the probabilities map carries 0.93 for the chosen
    // label. The verdict stands and records 0.93, which separates
    // the settled wire reading from a reading of the carried field,
    // under which the claim would have landed on its default.
    let outcome = run_fallback(Ok(verdict_body("roomy", probs("roomy", 0.93), 0.5)));
    assert_eq!(
        (outcome.record.port_start, outcome.record.port_end),
        (20000, 20039)
    );
    assert_eq!(outcome.line["verdict"], "roomy");
    assert_eq!(outcome.line["confidence"], 0.93);
    assert_eq!(outcome.line["fallback"], "none");
    assert_eq!(outcome.line["allocated"], "roomy");
}

#[test]
fn an_abstention_lands_on_the_default_at_any_confidence() {
    for confidence in [0.99, 0.1] {
        let outcome = run_fallback(Ok(verdict_body("other", probs("other", confidence), 0.5)));
        assert_eq!(
            (outcome.record.port_start, outcome.record.port_end),
            (20000, 20009)
        );
        assert_eq!(outcome.line["verdict"], "other");
        assert_eq!(outcome.line["confidence"], confidence);
        assert_eq!(outcome.line["fallback"], "abstention");
        assert_eq!(outcome.line["allocated"], "lean");
    }
}

#[test]
fn silence_and_malformed_and_transport_land_on_the_default_under_their_classes() {
    let outcome = run_fallback(Ok(String::new()));
    assert_eq!(outcome.line["fallback"], "silence");
    assert_eq!(outcome.line["verdict"], serde_json::Value::Null);
    assert_eq!(outcome.line["confidence"], serde_json::Value::Null);
    assert_eq!(outcome.line["allocated"], "lean");

    let outcome = run_fallback(Ok("{\"model\":\"jev-1.13.0\",\"answers\":{}}".to_string()));
    assert_eq!(outcome.line["fallback"], "silence");

    let one = verdict_body("lean", probs("lean", 0.9), 0.5);
    let two_documents = format!("{one}\n{one}");
    let outcome = run_fallback(Ok(two_documents));
    assert_eq!(outcome.line["fallback"], "malformed");
    assert_eq!(outcome.line["verdict"], serde_json::Value::Null);

    let outcome = run_fallback(Ok("{\"model\":\"jev-1.13.0\",\"answers\":".to_string()));
    assert_eq!(outcome.line["fallback"], "malformed");

    let mut out_of_range = probs("lean", 0.9);
    out_of_range[1].1 = 1.2;
    let outcome = run_fallback(Ok(verdict_body("lean", out_of_range, 0.5)));
    assert_eq!(outcome.line["fallback"], "malformed");

    let outcome = run_fallback(Err("the judge could not be reached".to_string()));
    assert_eq!(outcome.line["fallback"], "transport");
    assert_eq!(outcome.line["verdict"], serde_json::Value::Null);
    assert_eq!(outcome.line["allocated"], "lean");
    assert_eq!(
        (outcome.record.port_start, outcome.record.port_end),
        (20000, 20009)
    );
}

// SY-11, the command line surface

fn run_cli(args: &[&str]) -> (i32, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_switchyard"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn cli_stack(tag: &str) -> PathBuf {
    let stack = temp_dir(tag).join("stack");
    let (code, _, stderr) = run_cli(&["init", "--stack", stack.to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    stack
}

#[test]
fn a_profiled_claim_from_the_command_line() {
    let stack = cli_stack("cli-profile");
    let stack = stack.to_str().unwrap();
    let (code, stdout, stderr) =
        run_cli(&["claim", "alpha", "--stack", stack, "--profile", "standard"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20000-20019"), "{stdout}");
    let (code, _, stderr) = run_cli(&["claim", "beta", "--stack", stack, "--profile", "huge"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("unknown profile huge"), "{stderr}");
}

#[test]
fn the_routing_option_shapes_refuse_at_the_parser() {
    let stack = cli_stack("cli-shapes");
    let stack = stack.to_str().unwrap();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                "claim",
                "alpha",
                "--stack",
                stack,
                "--profile",
                "lean",
                "--describe",
                "work",
                "--default-profile",
                "lean",
            ],
            "--profile",
        ),
        (
            vec![
                "claim",
                "alpha",
                "--stack",
                stack,
                "--default-profile",
                "lean",
            ],
            "--default-profile",
        ),
        (
            vec![
                "claim",
                "alpha",
                "--stack",
                stack,
                "--judge-command",
                "/bin/true",
            ],
            "--judge-command",
        ),
        (
            vec!["claim", "alpha", "--stack", stack, "--describe", "work"],
            "--describe",
        ),
        (
            vec![
                "claim",
                "alpha",
                "--stack",
                stack,
                "--describe",
                "   ",
                "--default-profile",
                "lean",
            ],
            "--describe",
        ),
    ];
    for (args, named) in cases {
        let (code, _, stderr) = run_cli(&args);
        assert_eq!(code, 2, "{args:?} gave {stderr}");
        assert!(
            stderr.contains(&format!("argument {named}:")),
            "{args:?} gave {stderr}"
        );
    }
    // A routing option beside another command is unrecognized.
    let (code, _, stderr) = run_cli(&["list", "--stack", stack, "--describe", "work"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("unrecognized arguments"), "{stderr}");
}

#[test]
fn a_routed_claim_from_the_command_line_with_no_judge_command() {
    let stack = cli_stack("cli-routed-none");
    let stack_arg = stack.to_str().unwrap();
    let (code, stdout, stderr) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "roomy",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20000-20039"), "{stdout}");
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["fallback"], "no transport");
    assert_eq!(lines[0]["allocated"], "roomy");
}

fn write_judge_script(tag: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = temp_dir(tag).join("judge.sh");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn a_routed_claim_through_a_judge_command_and_through_a_failing_one() {
    let stack = cli_stack("cli-routed-judge");
    let stack_arg = stack.to_str().unwrap();
    let verdict = verdict_body("roomy", probs("roomy", 0.9), 0.5);
    let script = write_judge_script(
        "cli-judge-good",
        &format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{verdict}'\n"),
    );
    let (code, stdout, stderr) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "lean",
        "--judge-command",
        script.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20000-20039"), "{stdout}");
    let lines = read_log(&stack);
    assert_eq!(lines[0]["verdict"], "roomy");
    assert_eq!(lines[0]["fallback"], "none");

    let failing = write_judge_script("cli-judge-bad", "#!/bin/sh\nexit 3\n");
    let (code, stdout, stderr) = run_cli(&[
        "claim",
        "beta",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "lean",
        "--judge-command",
        failing.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20040-20049"), "{stdout}");
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["fallback"], "transport");
    assert_eq!(lines[1]["allocated"], "lean");

    let (code, _, stderr) = run_cli(&[
        "claim",
        "gamma",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "lean",
        "--judge-command",
        "/nonexistent/judge",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["fallback"], "transport");
}

#[test]
fn a_judge_that_answers_and_leaves_a_descendant_on_the_pipe_lands_on_the_default_at_the_deadline() {
    let stack = cli_stack("cli-judge-descendant");
    let stack_arg = stack.to_str().unwrap();
    let verdict = verdict_body("roomy", probs("roomy", 0.9), 0.5);
    let script = write_judge_script(
        "cli-judge-descendant",
        &format!("#!/bin/sh\nsleep 45 &\ncat > /dev/null\nprintf '%s' '{verdict}'\n"),
    );
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "lean",
        "--judge-command",
        script.to_str().unwrap(),
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20000-20009"), "{stdout}");
    assert!(
        elapsed >= std::time::Duration::from_secs(25),
        "the consultation returned in {elapsed:?}, before the deadline it was held to"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the consultation stretched to {elapsed:?}, past the deadline it was held to"
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["verdict"], serde_json::Value::Null);
    assert_eq!(lines[0]["fallback"], "transport");
    assert_eq!(lines[0]["allocated"], "lean");
}

#[test]
fn a_judge_that_sleeps_in_the_foreground_past_the_deadline_lands_on_the_default_at_the_deadline() {
    let stack = cli_stack("cli-judge-foreground");
    let stack_arg = stack.to_str().unwrap();
    let script = write_judge_script("cli-judge-foreground", "#!/bin/sh\nsleep 45 &\nsleep 45\n");
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_cli(&[
        "claim",
        "alpha",
        "--stack",
        stack_arg,
        "--describe",
        DESCRIPTION,
        "--default-profile",
        "lean",
        "--judge-command",
        script.to_str().unwrap(),
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ports 20000-20009"), "{stdout}");
    assert!(
        elapsed >= std::time::Duration::from_secs(25),
        "the consultation returned in {elapsed:?}, before the deadline it was held to"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(40),
        "the consultation stretched to {elapsed:?}, past the deadline it was held to"
    );
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["verdict"], serde_json::Value::Null);
    assert_eq!(lines[0]["fallback"], "transport");
    assert_eq!(lines[0]["allocated"], "lean");
}

// SY-16, the fixture transport

#[test]
fn the_fixture_transport_replays_by_digest_and_a_missing_reply_is_a_transport_landing() {
    let request = routing_request(DESCRIPTION);
    let digest = sha256_hex(request.as_bytes());
    let body = verdict_body("standard", probs("standard", 0.9), 0.5);
    let transport = FixtureTransport::from_pairs([(digest.clone(), body)]);
    assert_eq!(
        transport.consult(&request).unwrap(),
        verdict_body("standard", probs("standard", 0.9), 0.5)
    );

    let stack = default_stack("fixture-replay");
    let record = claim_routed(
        &stack,
        "alpha",
        DESCRIPTION,
        Profile::Lean,
        Some(&transport),
    )
    .unwrap();
    assert_eq!((record.port_start, record.port_end), (20000, 20019));

    let empty = FixtureTransport::new(std::collections::BTreeMap::new());
    let error = empty
        .consult(&request)
        .expect_err("a request with no recorded reply is a transport failure");
    assert!(
        error.contains("no recorded reply for the request digest"),
        "{error}"
    );
    assert!(error.contains(&digest), "{error}");

    let stack = default_stack("fixture-missing");
    let record = claim_routed(&stack, "alpha", DESCRIPTION, Profile::Lean, Some(&empty)).unwrap();
    assert_eq!((record.port_start, record.port_end), (20000, 20009));
    let lines = read_log(&stack);
    assert_eq!(lines[0]["fallback"], "transport");
}

// The recorded fixture replay groups. The fixtures under
// tests/fixtures/routing carry the exact request bytes the library
// built and the exact reply bytes the endpoint returned for the 48
// live cases, and hand built replies for the seven constructed
// failure cases, each with the routing log line and the claim
// outcome its reply settles. The manifest beside them states each
// case's provenance.

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/routing")
}

fn load_fixture(id: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(fixture_dir().join(format!("{id}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn all_fixture_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|id| id.starts_with("sw-"))
        .collect();
    ids.sort();
    ids
}

fn replay(fixture: &serde_json::Value) -> (PathBuf, switchyard::Record, serde_json::Value) {
    let pool = &fixture["pool"];
    let stack = stack_with(
        "replay",
        pool["start"].as_i64().unwrap() as i128,
        pool["range_size"].as_i64().unwrap() as i128,
        pool["range_count"].as_i64().unwrap() as i128,
    );
    let request = fixture["request"].as_str().unwrap();
    let transport = FixtureTransport::from_pairs([(
        sha256_hex(request.as_bytes()),
        fixture["reply"].as_str().unwrap().to_string(),
    )]);
    let default = profile_named(fixture["default_profile"].as_str().unwrap()).unwrap();
    let record = claim_routed(
        &stack,
        fixture["agent"].as_str().unwrap(),
        fixture["description"].as_str().unwrap(),
        default,
        Some(&transport),
    )
    .unwrap();
    let lines = read_log(&stack);
    assert_eq!(lines.len(), 1);
    (stack, record, lines.into_iter().next().unwrap())
}

#[test]
fn every_recorded_fixture_replays_to_its_recorded_outcome() {
    let ids = all_fixture_ids();
    assert_eq!(ids.len(), 55, "{ids:?}");
    for id in &ids {
        let fixture = load_fixture(id);
        assert_eq!(
            routing_request(fixture["description"].as_str().unwrap()),
            fixture["request"].as_str().unwrap(),
            "{id} request drifted from the library"
        );
        let (_, record, line) = replay(&fixture);
        let outcome = &fixture["expected_outcome"];
        assert_eq!(
            (record.port_start, record.port_end),
            (
                outcome["port_start"].as_i64().unwrap(),
                outcome["port_end"].as_i64().unwrap()
            ),
            "{id} span"
        );
        assert_eq!(
            record.agent,
            fixture["agent"].as_str().unwrap(),
            "{id} agent"
        );
        assert_eq!(log_field_set(&line), LOG_FIELDS, "{id} field set");
        let expected = &fixture["expected_log_line"];
        for key in LOG_FIELDS {
            if key == "at" {
                assert!(line["at"].as_str().unwrap().contains('T'), "{id} stamp");
                continue;
            }
            assert_eq!(line[key], expected[key], "{id} field {key}");
        }
    }
}

#[test]
fn the_request_document_is_pinned_byte_for_byte_for_one_description_of_each_band() {
    for id in ["sw-l03", "sw-s02", "sw-r01"] {
        let fixture = load_fixture(id);
        let request = routing_request(fixture["description"].as_str().unwrap());
        assert_eq!(request, fixture["request"].as_str().unwrap(), "{id}");
        let document: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(document["model"], "jev-1.13.0", "{id}");
        let question = &document["questions"]["profile"];
        assert_eq!(question["type"], "choice", "{id}");
        let state = document["state"].as_str().unwrap();
        assert!(
            state.starts_with("A furnishing tool has been asked to size one claim"),
            "{id} framing"
        );
        assert!(
            state.contains(fixture["description"].as_str().unwrap()),
            "{id} description verbatim"
        );
        assert!(
            state.ends_with("the description does not determine the count."),
            "{id} closing"
        );
    }
}

#[test]
fn a_mixed_stack_lists_plain_and_profiled_and_routed_claims_together() {
    let stack = default_stack("mixed");
    claim(&stack, "plain-one").unwrap();
    claim_profile(&stack, "profiled-one", Profile::Standard).unwrap();
    let fixture = load_fixture("sw-r01");
    let request = fixture["request"].as_str().unwrap();
    let transport = FixtureTransport::from_pairs([(
        sha256_hex(request.as_bytes()),
        fixture["reply"].as_str().unwrap().to_string(),
    )]);
    claim_routed(
        &stack,
        "routed-one",
        fixture["description"].as_str().unwrap(),
        Profile::Lean,
        Some(&transport),
    )
    .unwrap();
    let rows = switchyard::list_claims(&stack).unwrap();
    let summary: Vec<(&str, i64, i64)> = rows
        .iter()
        .map(|record| (record.agent.as_str(), record.port_start, record.port_end))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("plain-one", 20000, 20009),
            ("profiled-one", 20010, 20029),
            ("routed-one", 20030, 20069),
        ]
    );
    let registry: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap()).unwrap();
    assert_eq!(registry["version"], 1);
}

#[test]
fn registry_bytes_of_a_routed_claim_match_the_landed_writer_shape() {
    let fixture = load_fixture("sw-l03");
    let (stack, record, _) = replay(&fixture);
    let text = std::fs::read_to_string(stack.join(REGISTRY_NAME)).unwrap();
    let stamp = "\"claimed_at\": \"";
    let start = text.find(stamp).unwrap() + stamp.len();
    let end = text[start..].find('"').unwrap() + start;
    let blanked = format!("{}STAMP{}", &text[..start], &text[end..]);
    let agent = fixture["agent"].as_str().unwrap();
    let expected = serde_json::json!({
        "claims": {
            agent: {
                "agent": agent,
                "branch": format!("switchyard/{agent}"),
                "claimed_at": "STAMP",
                "database": format!("switchyard_{}", agent.replace('-', "_")),
                "env_file": format!("worktrees/{agent}/.env.switchyard"),
                "port_end": record.port_end,
                "port_start": record.port_start,
                "worktree": format!("worktrees/{agent}"),
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
