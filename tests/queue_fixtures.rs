//! Replay pins for the recorded wait queue corpus of obligation
//! SY-20.
//!
//! Each fixture under tests/fixtures/queue records one queue
//! state exactly as its vignette states it, the exact scoring
//! request bytes the library built for that state, the exact
//! reply the pinned model returned in the recording window, and
//! the wait record and queue log line the claim produced. The
//! replay restages the state on a synthetic stack, rebuilds the
//! request from the fixture facts and compares it byte for byte,
//! then drives the waiting claim through the fixture transport
//! keyed by the request digests and compares the wait record and
//! the queue log line field by field, the instant fields apart.
//! The corpus is the recorded evidence for the scoring doctrine,
//! and the replay keeps it honest offline.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use switchyard::{
    claim, claim_profile_waiting, claim_routed_waiting, claim_waiting, init_stack, load_registry,
    release, scoring_request, ClaimOutcome, FixtureTransport, Profile, Wait,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "switchyard-queue-fixtures-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixtures() -> Vec<serde_json::Value> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/queue");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().map(|ext| ext == "json").unwrap_or(false))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap())
        .collect()
}

fn draw_of(profile: &str) -> i64 {
    match profile {
        "standard" => 2,
        "roomy" => 4,
        _ => 1,
    }
}

/// The slot indices to leave free so the free runs stand as the
/// fixture states them, laid from the pool edge in the recording
/// driver's layout.
fn free_slot_indices(free_runs: &[i64]) -> Vec<i64> {
    let total_free: i64 = free_runs.iter().sum();
    let taken_total = 16 - total_free;
    let mut layout: Vec<bool> = Vec::new();
    let mut remaining_taken = taken_total;
    for (index, run) in free_runs.iter().enumerate() {
        layout.resize(layout.len() + (*run).max(0) as usize, true);
        if index + 1 < free_runs.len() {
            layout.push(false);
            remaining_taken -= 1;
        }
    }
    if remaining_taken > 0 {
        layout.resize(layout.len() + remaining_taken as usize, false);
    }
    layout
        .iter()
        .enumerate()
        .filter(|(_, free)| **free)
        .map(|(index, _)| index as i64)
        .collect()
}

/// Restage one fixture state. Filler claims shape the pool, and
/// the standing waits are written into the registry document in
/// the shape the recording driver wrote them.
fn stage(fixture: &serde_json::Value) -> PathBuf {
    let stack = temp_dir("stack").join("stack");
    init_stack(&stack, 20000, 10, 16).unwrap();
    for index in 0..16 {
        claim(&stack, &format!("filler-{index:02}")).unwrap();
    }
    let free_runs: Vec<i64> = fixture["free_runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run.as_i64().unwrap())
        .collect();
    for index in free_slot_indices(&free_runs) {
        release(&stack, &format!("filler-{index:02}")).unwrap();
    }
    let standing: Vec<String> = fixture["standing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|profile| profile.as_str().unwrap().to_string())
        .collect();
    if !standing.is_empty() {
        let path = stack.join("switchyard-registry.json");
        let mut document: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let waits: Vec<serde_json::Value> = standing
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                serde_json::json!({
                    "agent": format!("standing-{index:02}"),
                    "profile": profile,
                    "recorded_at": "2026-11-02T00:00:00+00:00",
                    "wait_for": null,
                    "score": null,
                    "confidence": null,
                    "model": null,
                    "fallback": "no transport",
                })
            })
            .collect();
        document
            .as_object_mut()
            .unwrap()
            .insert("waits".to_string(), serde_json::Value::Array(waits));
        fs::write(
            &path,
            serde_json::to_string_pretty(&document).unwrap() + "\n",
        )
        .unwrap();
    }
    stack
}

fn standing_waits(fixture: &serde_json::Value) -> Vec<Wait> {
    fixture["standing"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, profile)| Wait {
            agent: format!("standing-{index:02}"),
            profile: profile.as_str().unwrap().to_string(),
            recorded_at: "2026-11-02T00:00:00+00:00".to_string(),
            wait_for: None,
            score: None,
            confidence: None,
            model: None,
            fallback: "no transport".to_string(),
        })
        .collect()
}

fn read_lines(stack: &Path, name: &str) -> Vec<serde_json::Value> {
    let path = stack.join(name);
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_fields(actual: &serde_json::Value, expected: &serde_json::Value, what: &str) {
    let expected = expected.as_object().unwrap();
    for (key, want) in expected {
        let got = actual.get(key);
        assert_eq!(got, Some(want), "{what} field {key}");
    }
}

#[test]
fn the_recorded_queue_corpus_is_complete() {
    let all = fixtures();
    assert_eq!(all.len(), 56, "the queue corpus holds 56 fixtures");
    let live = all
        .iter()
        .filter(|fixture| fixture["provenance"] == "window")
        .count();
    let constructed = all
        .iter()
        .filter(|fixture| fixture["provenance"] == "constructed")
        .count();
    assert_eq!(live, 48);
    assert_eq!(constructed, 8);
}

#[test]
fn the_recorded_queue_fixtures_replay_exactly() {
    for fixture in fixtures() {
        let id = fixture["id"].as_str().unwrap().to_string();
        let stack = stage(&fixture);
        let settled = fixture["settled_profile"].as_str().unwrap();
        let bound = fixture["bound"].as_i64();
        let description = fixture["description"].as_str();
        let free_runs: Vec<i64> = fixture["free_runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run.as_i64().unwrap())
            .collect();
        let rebuilt = scoring_request(
            settled,
            draw_of(settled),
            &standing_waits(&fixture),
            &free_runs,
            bound,
            description,
        );
        assert_eq!(
            rebuilt,
            fixture["request"].as_str().unwrap(),
            "{id}: the rebuilt scoring request differs from the recorded bytes"
        );
        let mut replies: BTreeMap<String, String> = BTreeMap::new();
        if let (Some(request), Some(reply)) = (
            fixture["routing_request"].as_str(),
            fixture["routing_reply"].as_str(),
        ) {
            replies.insert(
                switchyard::sha256_hex(request.as_bytes()),
                reply.to_string(),
            );
        }
        replies.insert(
            switchyard::sha256_hex(fixture["request"].as_str().unwrap().as_bytes()),
            fixture["reply"].as_str().unwrap().to_string(),
        );
        let transport = FixtureTransport::new(replies);
        let agent = fixture["agent"].as_str().unwrap();
        let outcome = if fixture["routed"].as_bool().unwrap() {
            claim_routed_waiting(
                &stack,
                agent,
                description.unwrap(),
                Profile::from_name(settled).unwrap(),
                Some(&transport),
                bound,
            )
        } else if settled == "plain" {
            claim_waiting(&stack, agent, bound, Some(&transport))
        } else {
            claim_profile_waiting(
                &stack,
                agent,
                Profile::from_name(settled).unwrap(),
                bound,
                Some(&transport),
            )
        };
        let wait = match outcome.unwrap() {
            ClaimOutcome::Wait(wait) => wait,
            ClaimOutcome::Claim(record) => {
                panic!("{id}: the replay claim allocated for {}", record.agent)
            }
        };
        let expected = &fixture["expected_wait"];
        assert_eq!(wait.agent, expected["agent"].as_str().unwrap(), "{id}");
        assert_eq!(wait.profile, expected["profile"].as_str().unwrap(), "{id}");
        assert_eq!(wait.wait_for, expected["wait_for"].as_i64(), "{id}");
        assert_eq!(wait.score, expected["score"].as_i64(), "{id}");
        assert_eq!(wait.confidence, expected["confidence"].as_f64(), "{id}");
        assert_eq!(wait.model.as_deref(), expected["model"].as_str(), "{id}");
        assert_eq!(
            wait.fallback,
            expected["fallback"].as_str().unwrap(),
            "{id}"
        );
        let data = load_registry(&stack).unwrap();
        assert_eq!(data.waits.len(), standing_waits(&fixture).len() + 1, "{id}");
        let lines: Vec<serde_json::Value> = read_lines(&stack, "switchyard-queue.jsonl")
            .into_iter()
            .filter(|line| line["agent"] == agent)
            .collect();
        assert_eq!(lines.len(), 1, "{id}: queue lines for the agent");
        assert_fields(
            &lines[0],
            &fixture["expected_queue_line"],
            &format!("{id} queue line"),
        );
        if let Some(expected) = fixture["expected_routing_line"].as_object() {
            let lines = read_lines(&stack, "switchyard-routing.jsonl");
            assert_eq!(lines.len(), 1, "{id}: routing lines");
            for (key, want) in expected {
                assert_eq!(
                    lines[0].get(key),
                    Some(want),
                    "{id} routing line field {key}"
                );
            }
        }
    }
}
