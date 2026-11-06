//! Core allocation logic for switchyard.
//!
//! A stack is a plain directory. Inside it live a git repository, a
//! worktrees directory, a lock file, and one registry file that
//! records the port pool and every live claim. The registry is the
//! only record, and every mutating command holds an exclusive flock
//! lock on the lock file across the whole read and check and modify
//! and write of the registry, so racing processes cannot both take
//! the same agent name or overlapping port ranges.

pub mod cli;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

pub const REGISTRY_NAME: &str = "switchyard-registry.json";
pub const ROUTING_LOG_NAME: &str = "switchyard-routing.jsonl";
/// The queue log of obligation SY-23, an append-only evidence
/// document that no command reads to decide anything.
pub const QUEUE_LOG_NAME: &str = "switchyard-queue.jsonl";
/// The most waits that may stand in one stack, the bound of
/// obligation SY-17.
pub const QUEUE_BOUND: usize = 64;
pub const LOCK_NAME: &str = "switchyard.lock";
pub const REPO_NAME: &str = "repo";
pub const WORKTREES_NAME: &str = "worktrees";
pub const ENV_NAME: &str = ".env.switchyard";
pub const REGISTRY_VERSION: i64 = 1;

pub const DEFAULT_PORT_START: i64 = 20000;
pub const DEFAULT_RANGE_SIZE: i64 = 10;
pub const DEFAULT_RANGE_COUNT: i64 = 16;

pub const AGENT_NAME_MAX: usize = 40;
pub const GIT_TIMEOUT_SECONDS: u64 = 120;

/// The model the routed entry point pins in every request it
/// builds. A request naming any other model is never built,
/// because the routing log must name a model that was actually
/// consulted.
pub const ROUTING_MODEL: &str = "jev-1.13.0";

/// The name of the one question a routing request carries.
pub const ROUTING_QUESTION: &str = "profile";

/// The bound on a routed claim's job description, in UTF-8 bytes.
pub const DESCRIPTION_MAX_BYTES: usize = 4096;

/// The confidence bar a verdict must clear to stand. A verdict
/// below it lands the claim on its declared default profile under
/// the low confidence class of obligation SY-14.
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.5;

/// The timeout the subprocess judge transport runs under, in
/// seconds, fixed here so every caller shares the one bound.
pub const JUDGE_TIMEOUT_SECONDS: u64 = 30;

/// The four labels a verdict document may choose among, the three
/// declared profiles and the none of these label the wire names
/// other.
pub const VERDICT_LABELS: [&str; 4] = ["lean", "standard", "roomy", "other"];

/// An error the CLI reports with a nonzero exit and no traceback.
/// The variants carry the exit codes the Python implementation's
/// exception hierarchy carried, one variant per exit code shape.
#[derive(Debug)]
pub enum Error {
    /// A plain failure, reported at exit 1.
    Plain(String),
    /// The stack root holds no registry, reported at exit 2.
    NotInitialised(String),
    /// The registry cannot be parsed or its shape is wrong, exit 2.
    Corrupt(String),
    /// The agent name is not one path safe component, exit 2.
    InvalidName(String),
    /// A raw operating system failure inside stack initialisation,
    /// wrapped by the caller that knows the stack path.
    Io(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Plain(_) | Error::Io(_) => 1,
            Error::NotInitialised(_) | Error::Corrupt(_) | Error::InvalidName(_) => 2,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Error::Plain(text)
            | Error::NotInitialised(text)
            | Error::Corrupt(text)
            | Error::InvalidName(text)
            | Error::Io(text) => text,
        }
    }

    fn plain(message: impl Into<String>) -> Error {
        Error::Plain(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn corrupt(path: &Path, why: impl fmt::Display) -> Error {
    Error::Corrupt(format!("registry at {} is corrupt, {why}", path.display()))
}

/// Quote a string the way Python's repr quotes it, for the error
/// messages that interpolate a caller supplied agent name.
fn py_repr(text: &str) -> String {
    let mut out = String::from("'");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 || (ch as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('\'');
    out
}

fn is_agent_name_char(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

/// The agent name pattern, one path safe component of lowercase
/// letters and digits and hyphens, opening and closing on a letter
/// or a digit, at most AGENT_NAME_MAX characters long.
pub fn validate_agent_name(agent: &str) -> Result<()> {
    let bytes = agent.as_bytes();
    let shape = !bytes.is_empty()
        && is_agent_name_char(bytes[0])
        && is_agent_name_char(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| is_agent_name_char(*byte) || *byte == b'-');
    if agent.chars().count() > AGENT_NAME_MAX || !shape {
        return Err(Error::InvalidName(format!(
            "invalid agent name {}, use one path safe component of lowercase \
             letters and digits and hyphens, at most {AGENT_NAME_MAX} characters long",
            py_repr(agent)
        )));
    }
    Ok(())
}

pub fn branch_name(agent: &str) -> String {
    format!("switchyard/{agent}")
}

pub fn database_name(agent: &str) -> String {
    format!("switchyard_{}", agent.replace('-', "_"))
}

/// Resolve a path the way Python's Path.resolve resolves it. The
/// longest standing prefix is canonicalised and the rest is applied
/// lexically, so a path that does not stand yet still resolves.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut existing = absolute.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut resolved = fs::canonicalize(&existing).unwrap_or(existing);
    for name in tail.iter().rev() {
        if name == ".." {
            resolved.pop();
        } else if name != "." {
            resolved.push(name);
        }
    }
    resolved
}

/// One recorded claim, carrying exactly the fields the registry
/// format records.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub agent: String,
    pub branch: String,
    pub worktree: String,
    pub port_start: i64,
    pub port_end: i64,
    pub database: String,
    pub env_file: String,
    pub claimed_at: String,
}

/// The port pool a stack allocates ranges from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pool {
    pub start: i64,
    pub range_size: i64,
    pub range_count: i64,
}

/// The registry document, the only record of who holds what.
#[derive(Debug, Clone, PartialEq)]
pub struct Registry {
    pub version: i64,
    pub port_pool: Pool,
    pub claims: BTreeMap<String, Record>,
    /// The standing waits of obligation SY-18, in the order they
    /// were recorded. The registry document carries the waits key
    /// exactly while this list stands non-empty, under the
    /// presence rule of that obligation.
    pub waits: Vec<Wait>,
}

/// One recorded wait, carrying exactly the fields obligation
/// SY-18 records and no others.
#[derive(Debug, Clone, PartialEq)]
pub struct Wait {
    pub agent: String,
    /// The settled profile word, one of plain and lean and
    /// standard and roomy.
    pub profile: String,
    pub recorded_at: String,
    pub wait_for: Option<i64>,
    pub score: Option<i64>,
    pub confidence: Option<f64>,
    pub model: Option<String>,
    /// The fallback word of obligation SY-20, one of the seven
    /// scoring vocabulary words, which is none exactly where a
    /// score stands.
    pub fallback: String,
}

/// The draw in ranges of a settled profile word, where plain
/// settles on its own word with a draw of one range, the same
/// size as lean and a different kind.
fn settled_draw(profile: &str) -> i64 {
    match profile {
        "standard" => 2,
        "roomy" => 4,
        _ => 1,
    }
}

/// A parsed JSON value that keeps object pairs in document order,
/// so the registry reader can reject a repeated key the way the
/// Python reader did instead of letting a later value win quietly.
#[derive(Debug, Clone, PartialEq)]
enum JVal {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    fn as_int(&self) -> Option<i64> {
        match self {
            JVal::Int(value) => Some(*value),
            JVal::UInt(value) => i64::try_from(*value).ok(),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&JVal> {
        match self {
            JVal::Obj(pairs) => pairs.iter().find(|(name, _)| name == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            JVal::Str(text) => Some(text),
            _ => None,
        }
    }
}

struct JValVisitor;

impl<'de> Visitor<'de> for JValVisitor {
    type Value = JVal;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<JVal, E> {
        Ok(JVal::Bool(value))
    }

    fn visit_unit<E>(self) -> std::result::Result<JVal, E> {
        Ok(JVal::Null)
    }

    fn visit_none<E>(self) -> std::result::Result<JVal, E> {
        Ok(JVal::Null)
    }

    fn visit_some<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<JVal, D::Error> {
        JVal::deserialize(deserializer)
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<JVal, E> {
        Ok(JVal::Int(value))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<JVal, E> {
        Ok(JVal::UInt(value))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<JVal, E> {
        Ok(JVal::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<JVal, E> {
        Ok(JVal::Str(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<JVal, E> {
        Ok(JVal::Str(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<JVal, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(JVal::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<JVal, A::Error> {
        let mut pairs = Vec::new();
        while let Some((key, value)) = map.next_entry()? {
            pairs.push((key, value));
        }
        Ok(JVal::Obj(pairs))
    }
}

impl<'de> Deserialize<'de> for JVal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<JVal, D::Error> {
        deserializer.deserialize_any(JValVisitor)
    }
}

/// The first repeated key in the document, checked the way
/// Python's object pairs hook fired, innermost objects first and
/// each object in document order.
fn first_duplicate_key(value: &JVal) -> Option<String> {
    match value {
        JVal::Arr(items) => {
            for item in items {
                if let Some(key) = first_duplicate_key(item) {
                    return Some(key);
                }
            }
            None
        }
        JVal::Obj(pairs) => {
            for (_, item) in pairs {
                if let Some(key) = first_duplicate_key(item) {
                    return Some(key);
                }
            }
            let mut seen = std::collections::HashSet::new();
            for (key, _) in pairs {
                if !seen.insert(key) {
                    return Some(key.clone());
                }
            }
            None
        }
        _ => None,
    }
}

const REGISTRY_KEYS: [&str; 3] = ["version", "port_pool", "claims"];
const POOL_KEYS: [&str; 3] = ["start", "range_size", "range_count"];
const RECORD_KEYS: [&str; 8] = [
    "agent",
    "branch",
    "worktree",
    "port_start",
    "port_end",
    "database",
    "env_file",
    "claimed_at",
];
/// The field set of one recorded wait, of obligation SY-18.
const WAIT_KEYS: [&str; 8] = [
    "agent",
    "profile",
    "recorded_at",
    "wait_for",
    "score",
    "confidence",
    "model",
    "fallback",
];
/// The settled profile words a wait may carry, of obligation
/// SY-18.
const SETTLED_PROFILES: [&str; 4] = ["plain", "lean", "standard", "roomy"];
/// The fallback vocabulary of a wait record, of obligation
/// SY-20. A wait that carries a score carries the word none, and
/// every other wait carries the class that left it scoreless.
const SCORE_FALLBACKS: [&str; 7] = [
    "none",
    "low confidence",
    "abstention",
    "malformed",
    "silence",
    "transport",
    "no transport",
];

fn exact_keys(pairs: &[(String, JVal)], expected: &[&str]) -> bool {
    pairs.len() == expected.len()
        && expected
            .iter()
            .all(|key| pairs.iter().any(|(name, _)| name == key))
}

fn derived_fields(agent: &str) -> [(&'static str, String); 4] {
    let worktree = format!("{WORKTREES_NAME}/{agent}");
    [
        ("branch", branch_name(agent)),
        ("worktree", worktree.clone()),
        ("database", database_name(agent)),
        ("env_file", format!("{worktree}/{ENV_NAME}")),
    ]
}

/// The last port of a pool, computed wide with checked
/// multiplication, so pool numbers of any magnitude the caller or
/// the file can carry resolve to a value or to no value, and never
/// wrap into a small plausible port. Python computed this in
/// unbounded integers and refused any pool reaching past the port
/// limit, so no value here means exactly that refusal.
fn pool_end_wide(start: i128, range_size: i128, range_count: i128) -> Option<i128> {
    range_size
        .checked_mul(range_count)
        .and_then(|span| start.checked_add(span))
        .map(|end| end - 1)
}

fn validate_registry(data: &JVal, path: &Path) -> Result<Registry> {
    let pairs = match data {
        JVal::Obj(pairs) => pairs,
        _ => return Err(corrupt(path, "the top level is not an object")),
    };
    let mut top_keys: Vec<&str> = REGISTRY_KEYS.to_vec();
    if data.get("waits").is_some() {
        top_keys.push("waits");
    }
    if !exact_keys(pairs, &top_keys) {
        return Err(corrupt(
            path,
            "the top level fields are not exactly version and port_pool and claims, with waits standing only where a wait stands",
        ));
    }
    let version = data.get("version").unwrap();
    if version.as_int() != Some(REGISTRY_VERSION) {
        return Err(corrupt(path, "the version is missing or unsupported"));
    }
    let pool_value = data.get("port_pool").unwrap();
    let pool_pairs = match pool_value {
        JVal::Obj(pairs) => pairs,
        _ => return Err(corrupt(path, "the port pool is missing")),
    };
    if !exact_keys(pool_pairs, &POOL_KEYS) {
        return Err(corrupt(
            path,
            "the port pool fields are not exactly start and range_size and range_count",
        ));
    }
    let mut pool_ints = [0i64; 3];
    for (slot, key) in pool_ints.iter_mut().zip(POOL_KEYS.iter()) {
        match pool_value.get(key).and_then(JVal::as_int) {
            Some(value) if value > 0 => *slot = value,
            _ => {
                return Err(corrupt(
                    path,
                    format!("the port pool field {key} is not a positive integer"),
                ))
            }
        }
    }
    let pool = Pool {
        start: pool_ints[0],
        range_size: pool_ints[1],
        range_count: pool_ints[2],
    };
    let pool_end = match pool_end_wide(
        pool.start as i128,
        pool.range_size as i128,
        pool.range_count as i128,
    ) {
        Some(end) if end <= 65535 => end as i64,
        _ => return Err(corrupt(path, "the port pool reaches past port 65535")),
    };
    let claims_value = data.get("claims").unwrap();
    let claim_pairs = match claims_value {
        JVal::Obj(pairs) => pairs,
        _ => return Err(corrupt(path, "the claims are missing or not an object")),
    };
    let mut records = Vec::new();
    let mut ranges: Vec<(i64, i64, String)> = Vec::new();
    for (agent, record_value) in claim_pairs {
        if validate_agent_name(agent).is_err() {
            return Err(corrupt(
                path,
                format!("the claim key {agent} is not a safe agent name"),
            ));
        }
        let record_pairs = match record_value {
            JVal::Obj(pairs) => pairs,
            _ => {
                return Err(corrupt(
                    path,
                    format!("the claim for {agent} is not an object"),
                ))
            }
        };
        if !exact_keys(record_pairs, &RECORD_KEYS) {
            return Err(corrupt(
                path,
                format!("the claim for {agent} does not carry exactly the recorded fields"),
            ));
        }
        let field = |key: &str| record_value.get(key).unwrap();
        if field("agent").as_str() != Some(agent.as_str()) {
            return Err(corrupt(
                path,
                format!("the claim keyed {agent} names a different agent"),
            ));
        }
        for key in ["branch", "worktree", "database", "env_file", "claimed_at"] {
            match field(key).as_str() {
                Some(text) if !text.is_empty() => {}
                _ => {
                    return Err(corrupt(
                        path,
                        format!("the claim for {agent} has no sound {key}"),
                    ))
                }
            }
        }
        for (key, expected) in derived_fields(agent) {
            let actual = field(key).as_str().unwrap();
            if actual != expected {
                return Err(corrupt(
                    path,
                    format!(
                        "the claim for {agent} records {key} as {actual} \
                         where the agent name derives {expected}"
                    ),
                ));
            }
        }
        let claimed_at = field("claimed_at").as_str().unwrap().to_string();
        if !is_valid_timestamp(&claimed_at) {
            return Err(corrupt(
                path,
                format!(
                    "the claim for {agent} has a claimed_at that does not parse as a timestamp"
                ),
            ));
        }
        let mut ports = [0i64; 2];
        for (slot, key) in ports.iter_mut().zip(["port_start", "port_end"].iter()) {
            match field(key).as_int() {
                Some(value) => *slot = value,
                None => {
                    return Err(corrupt(
                        path,
                        format!("the claim for {agent} has no sound {key}"),
                    ))
                }
            }
        }
        let (port_start, port_end) = (ports[0], ports[1]);
        if port_start > port_end {
            return Err(corrupt(
                path,
                format!("the claim for {agent} has an inverted port range"),
            ));
        }
        if port_start < pool.start || port_end > pool_end {
            return Err(corrupt(
                path,
                format!("the claim for {agent} reaches outside the port pool"),
            ));
        }
        // A record holds whole slots of the grid, aligned at its
        // start. Round 1 records hold exactly one slot, and the
        // routing slice of Round 3 grows the span a record may
        // carry to the consecutive slots a profile drew, which
        // obligation SY-15 names as the one growth of the format.
        let span = port_end - port_start + 1;
        if (port_start - pool.start) % pool.range_size != 0 || span % pool.range_size != 0 {
            return Err(corrupt(
                path,
                format!("the claim for {agent} does not hold whole slots of the port pool grid"),
            ));
        }
        ranges.push((port_start, port_end, agent.clone()));
        records.push(Record {
            agent: agent.clone(),
            branch: field("branch").as_str().unwrap().to_string(),
            worktree: field("worktree").as_str().unwrap().to_string(),
            port_start,
            port_end,
            database: field("database").as_str().unwrap().to_string(),
            env_file: field("env_file").as_str().unwrap().to_string(),
            claimed_at,
        });
    }
    ranges.sort();
    for pair in ranges.windows(2) {
        let (prev, current) = (&pair[0], &pair[1]);
        if current.0 <= prev.1 {
            return Err(corrupt(
                path,
                format!(
                    "the claims for {} and {} hold overlapping ranges",
                    prev.2, current.2
                ),
            ));
        }
    }
    let claims = records
        .into_iter()
        .map(|record| (record.agent.clone(), record))
        .collect();
    let waits = match data.get("waits") {
        Some(value) => validate_waits(value, &claims, path)?,
        None => Vec::new(),
    };
    Ok(Registry {
        version: REGISTRY_VERSION,
        port_pool: pool,
        claims,
        waits,
    })
}

/// Validate the waits key of a registry document, under
/// obligation SY-18. A document whose waits do not carry exactly
/// the fields that obligation names, or whose waits stand in a
/// shape no writer of this slice writes, is corrupt, and the
/// reader refuses it whole.
fn validate_waits(
    value: &JVal,
    claims: &BTreeMap<String, Record>,
    path: &Path,
) -> Result<Vec<Wait>> {
    let entries = match value {
        JVal::Arr(entries) => entries,
        _ => return Err(corrupt(path, "the waits are not a list")),
    };
    if entries.len() > QUEUE_BOUND {
        return Err(corrupt(
            path,
            format!("more than {QUEUE_BOUND} waits stand in the registry"),
        ));
    }
    let mut waits: Vec<Wait> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        let pairs = match entry {
            JVal::Obj(pairs) => pairs,
            _ => return Err(corrupt(path, "a wait is not an object")),
        };
        if !exact_keys(pairs, &WAIT_KEYS) {
            return Err(corrupt(
                path,
                "the fields of a wait are not exactly the fields obligation SY-18 names",
            ));
        }
        let agent = match entry.get("agent").and_then(JVal::as_str) {
            Some(agent) if validate_agent_name(agent).is_ok() => agent.to_string(),
            _ => return Err(corrupt(path, "a wait carries an agent name SY-10 refuses")),
        };
        if !seen.insert(agent.clone()) {
            return Err(corrupt(
                path,
                format!("the agent {agent} stands in two waits"),
            ));
        }
        if claims.contains_key(&agent) {
            return Err(corrupt(
                path,
                format!("the agent {agent} stands in both a claim and a wait"),
            ));
        }
        let profile = match entry.get("profile").and_then(JVal::as_str) {
            Some(profile) if SETTLED_PROFILES.contains(&profile) => profile.to_string(),
            _ => return Err(corrupt(path, "a wait carries a settled profile outside the four")),
        };
        let recorded_at = match entry.get("recorded_at").and_then(JVal::as_str) {
            Some(stamp) if is_valid_timestamp(stamp) => stamp.to_string(),
            _ => return Err(corrupt(path, "a wait carries a recorded instant that is not a timestamp")),
        };
        let wait_for = match entry.get("wait_for") {
            Some(JVal::Null) => None,
            Some(value) => match value.as_int() {
                Some(bound) if bound > 0 => Some(bound),
                _ => {
                    return Err(corrupt(
                        path,
                        "a wait carries a wait bound that is not a positive number of seconds",
                    ))
                }
            },
            None => return Err(corrupt(path, "a wait carries no wait bound field")),
        };
        let score = match entry.get("score") {
            Some(JVal::Null) => None,
            Some(value) => match value.as_int() {
                Some(level) if (1..=10).contains(&level) => Some(level),
                _ => return Err(corrupt(path, "a wait carries a score outside 1 to 10")),
            },
            None => return Err(corrupt(path, "a wait carries no score field")),
        };
        let confidence = match entry.get("confidence") {
            Some(JVal::Null) => None,
            Some(value) => match jval_f64(value) {
                Some(number) if (0.0..=1.0).contains(&number) => Some(number),
                _ => {
                    return Err(corrupt(
                        path,
                        "a wait carries a confidence outside 0 to 1",
                    ))
                }
            },
            None => return Err(corrupt(path, "a wait carries no confidence field")),
        };
        if score.is_some() != confidence.is_some() {
            return Err(corrupt(
                path,
                "a wait carries a score without its confidence or a confidence without its score",
            ));
        }
        let model = match entry.get("model") {
            Some(JVal::Null) => None,
            Some(value) => match value.as_str() {
                Some(name) if !name.is_empty() => Some(name.to_string()),
                _ => return Err(corrupt(path, "a wait carries a model that is not a name")),
            },
            None => return Err(corrupt(path, "a wait carries no model field")),
        };
        let fallback = match entry.get("fallback").and_then(JVal::as_str) {
            Some(word) if SCORE_FALLBACKS.contains(&word) => word.to_string(),
            _ => {
                return Err(corrupt(
                    path,
                    "a wait carries a fallback word outside the vocabulary of SY-20",
                ))
            }
        };
        if score.is_some() != (fallback == "none") {
            return Err(corrupt(
                path,
                "a wait carries a score beside a fallback word that is not none, or no score beside the word none",
            ));
        }
        waits.push(Wait {
            agent,
            profile,
            recorded_at,
            wait_for,
            score,
            confidence,
            model,
            fallback,
        });
    }
    Ok(waits)
}

/// Read and validate the registry, changing nothing on disk.
pub fn load_registry(stack: &Path) -> Result<Registry> {
    let path = stack.join(REGISTRY_NAME);
    if !path.exists() {
        return Err(Error::NotInitialised(format!(
            "no registry at {}, run switchyard init to create the stack",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(corrupt(&path, "the registry path is not a file"));
    }
    let raw = match fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(exc) => {
                return Err(corrupt(&path, format!("it is not valid UTF-8 text, {exc}")));
            }
        },
        Err(exc) => {
            return Err(corrupt(&path, format!("it cannot be read, {exc}")));
        }
    };
    let data: JVal = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(exc) => {
            if exc.to_string().contains("recursion limit") {
                return Err(corrupt(&path, "it nests too deeply to be read"));
            }
            return Err(corrupt(&path, format!("it is not valid JSON, {exc}")));
        }
    };
    if let Some(key) = first_duplicate_key(&data) {
        return Err(corrupt(&path, format!("it repeats the key {key}")));
    }
    validate_registry(&data, &path)
}

/// The JSON object of one recorded wait, carrying exactly the
/// fields obligation SY-18 names.
fn wait_value(wait: &Wait) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    entry.insert("agent".to_string(), wait.agent.clone().into());
    entry.insert(
        "confidence".to_string(),
        match wait.confidence {
            Some(number) => serde_json::json!(number),
            None => serde_json::Value::Null,
        },
    );
    entry.insert("fallback".to_string(), wait.fallback.clone().into());
    entry.insert(
        "model".to_string(),
        match &wait.model {
            Some(model) => model.clone().into(),
            None => serde_json::Value::Null,
        },
    );
    entry.insert("profile".to_string(), wait.profile.clone().into());
    entry.insert("recorded_at".to_string(), wait.recorded_at.clone().into());
    entry.insert(
        "score".to_string(),
        match wait.score {
            Some(level) => level.into(),
            None => serde_json::Value::Null,
        },
    );
    entry.insert(
        "wait_for".to_string(),
        match wait.wait_for {
            Some(bound) => bound.into(),
            None => serde_json::Value::Null,
        },
    );
    serde_json::Value::Object(entry)
}

fn registry_value(data: &Registry) -> serde_json::Value {
    let mut claims = serde_json::Map::new();
    for (agent, record) in &data.claims {
        let mut entry = serde_json::Map::new();
        entry.insert("agent".to_string(), record.agent.clone().into());
        entry.insert("branch".to_string(), record.branch.clone().into());
        entry.insert("claimed_at".to_string(), record.claimed_at.clone().into());
        entry.insert("database".to_string(), record.database.clone().into());
        entry.insert("env_file".to_string(), record.env_file.clone().into());
        entry.insert("port_end".to_string(), record.port_end.into());
        entry.insert("port_start".to_string(), record.port_start.into());
        entry.insert("worktree".to_string(), record.worktree.clone().into());
        claims.insert(agent.clone(), serde_json::Value::Object(entry));
    }
    let mut pool = serde_json::Map::new();
    pool.insert("range_count".to_string(), data.port_pool.range_count.into());
    pool.insert("range_size".to_string(), data.port_pool.range_size.into());
    pool.insert("start".to_string(), data.port_pool.start.into());
    let mut top = serde_json::Map::new();
    top.insert("claims".to_string(), serde_json::Value::Object(claims));
    top.insert("port_pool".to_string(), serde_json::Value::Object(pool));
    top.insert("version".to_string(), data.version.into());
    if !data.waits.is_empty() {
        let waits: Vec<serde_json::Value> = data.waits.iter().map(wait_value).collect();
        top.insert("waits".to_string(), serde_json::Value::Array(waits));
    }
    serde_json::Value::Object(top)
}

/// A random suffix for one save's temporary file, drawn fresh on
/// every call, so the name is never standing in the stack before
/// the save creates it.
fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    match getrandom::fill(&mut bytes) {
        Ok(()) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        Err(_) => {
            // The random source failed, so fall back to the clock,
            // the process id, and a counter. The create_new open in
            // the save still refuses any name that already stands,
            // so a collision here costs a retry and never becomes
            // a write through a planted name.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tick = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|span| span.as_nanos() as u64)
                .unwrap_or(0);
            let mixed = nanos
                ^ (std::process::id() as u64) << 32
                ^ tick.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            format!("{mixed:016x}")
        }
    }
}

/// Save the registry by rename from a sibling temporary file, so a
/// reader never sees a half written registry.
///
/// The temporary file carries a random suffix drawn for this save
/// alone and is created with create_new, so the save never opens a
/// name that already stands in the stack. A fixed temporary name
/// would be a plant target, because a link left at that name would
/// redirect the write through itself onto whatever it points at
/// and the rename would then leave the registry path as that link.
/// The save removes its own temporary file on every failure path.
pub fn save_registry(stack: &Path, data: &Registry) -> Result<()> {
    let path = stack.join(REGISTRY_NAME);
    let text = serde_json::to_string_pretty(&registry_value(data)).unwrap() + "\n";
    let mut attempt = 0;
    let result = loop {
        attempt += 1;
        let tmp = stack.join(format!("{REGISTRY_NAME}.tmp-{}", random_suffix()));
        let written = (|| -> io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            drop(file);
            fs::rename(&tmp, &path)
        })();
        match written {
            Ok(()) => break Ok(()),
            Err(exc) => {
                let _ = fs::remove_file(&tmp);
                if exc.kind() == io::ErrorKind::AlreadyExists && attempt < 8 {
                    continue;
                }
                break Err(exc);
            }
        }
    };
    result.map_err(|exc| {
        Error::plain(format!(
            "the registry at {} cannot be written, {exc}",
            path.display()
        ))
    })
}

/// The stack lock, held across the whole read and check and modify
/// and write of the registry.
///
/// The lock is a file identity and not a path name. A lock file
/// deleted and recreated at the same path while a command holds
/// the old file gives the next command a different file to lock,
/// and two holders of two different files exclude nobody. So the
/// holder confirms the file it holds is the file the lock path
/// still names, once after taking the lock and again immediately
/// before publishing the registry, and a mismatch aborts the
/// command with the registry untouched.
struct Locked {
    file: fs::File,
    lock_path: PathBuf,
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_identity_error(lock_path: &Path) -> Error {
    Error::plain(format!(
        "the lock file at {} is not the file this command locked, \
         so the registry was left unchanged",
        lock_path.display()
    ))
}

impl Locked {
    /// Confirm the lock path still names the very file this lock
    /// holds, by device and inode on both sides.
    fn verify_current(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let held = match self.file.metadata() {
            Ok(meta) => meta,
            Err(_) => return Err(lock_identity_error(&self.lock_path)),
        };
        let same = match fs::metadata(&self.lock_path) {
            Ok(meta) => meta.dev() == held.dev() && meta.ino() == held.ino(),
            Err(_) => false,
        };
        if same {
            Ok(())
        } else {
            Err(lock_identity_error(&self.lock_path))
        }
    }
}

/// How many times taking the lock retries when the lock file is
/// replaced between the open and the identity check.
const LOCK_ACQUIRE_ATTEMPTS: usize = 5;

fn locked(stack: &Path) -> Result<Locked> {
    let lock_path = stack.join(LOCK_NAME);
    for _ in 0..LOCK_ACQUIRE_ATTEMPTS {
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(exc) if exc.kind() == io::ErrorKind::NotFound => recreate_lock(stack, &lock_path)?,
            Err(exc) => {
                return Err(Error::plain(format!(
                    "the lock file at {} cannot be opened, {exc}",
                    lock_path.display()
                )));
            }
        };
        file.lock_exclusive().map_err(|exc| {
            Error::plain(format!(
                "the lock file at {} cannot be locked, {exc}",
                lock_path.display()
            ))
        })?;
        let held = Locked {
            file,
            lock_path: lock_path.clone(),
        };
        if held.verify_current().is_ok() {
            return Ok(held);
        }
    }
    Err(Error::plain(format!(
        "the lock file at {} kept being replaced while it was being locked, \
         so nothing was changed",
        lock_path.display()
    )))
}

/// Recreate a missing lock file when the registry survives.
///
/// The lock file is an empty marker whose only job is to be
/// flocked, so a stack whose registry is intact but whose lock file
/// was deleted recovers by recreating the marker. The registry is
/// loaded and validated before the marker is created, so a stack
/// with no registry is reported as uninitialised and a stack whose
/// registry is corrupt is reported as corrupt, and neither report
/// leaves a new lock file behind.
fn recreate_lock(stack: &Path, lock_path: &Path) -> Result<fs::File> {
    load_registry(stack)?;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(lock_path)
        .map_err(|exc| {
            Error::plain(format!(
                "the lock file at {} cannot be recreated, {exc}",
                lock_path.display()
            ))
        })
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The instant a claim is made, stamped the way Python's
/// datetime.now(timezone.utc).isoformat stamps it.
fn now_stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    if micros == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
    }
}

/// The inverse of civil_from_days, the number of days from the
/// civil date to the epoch, by the same era arithmetic.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = if shifted >= 0 { shifted } else { shifted - 399 } / 400;
    let yoe = shifted - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The epoch microseconds a timestamp names, for a timestamp in
/// the grammar is_valid_timestamp accepts. The registry reader
/// validates every recorded instant against that grammar before
/// an expiry comparison reads it, so a value outside the grammar
/// never reaches this parser from a validated document.
fn stamp_micros(text: &str) -> Option<i128> {
    let bytes = text.as_bytes();
    let (year, mut at) = take_digits(bytes, 0, 4)?;
    let extended = bytes.get(at) == Some(&b'-');
    if extended {
        at += 1;
    }
    let (month, next) = take_digits(bytes, at, 2)?;
    at = next;
    if extended {
        if bytes.get(at) != Some(&b'-') {
            return None;
        }
        at += 1;
    }
    let (day, next) = take_digits(bytes, at, 2)?;
    at = next;
    let days = days_from_civil(year as i64, month, day) as i128;
    // A date alone stands at the start of its day.
    let Some(&separator) = bytes.get(at) else {
        return Some(days * 86_400_000_000);
    };
    let _ = separator;
    at += 1;
    let (hour, next) = take_digits(bytes, at, 2)?;
    at = next;
    let basic_time = bytes.get(at) != Some(&b':');
    if !basic_time {
        at += 1;
    }
    let (minute, next) = take_digits(bytes, at, 2)?;
    at = next;
    let mut second: u32 = 0;
    if !basic_time {
        if bytes.get(at) != Some(&b':') {
            return None;
        }
        at += 1;
        let (parsed, next) = take_digits(bytes, at, 2)?;
        second = parsed;
        at = next;
    } else if let Some((parsed, next)) = take_digits(bytes, at, 2) {
        second = parsed;
        at = next;
    }
    let mut micros: i128 = 0;
    if matches!(bytes.get(at), Some(b'.') | Some(b',')) {
        at += 1;
        let start = at;
        let mut value: i128 = 0;
        while let Some(&byte) = bytes.get(at) {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value * 10 + (byte - b'0') as i128;
            at += 1;
        }
        let digits = (at - start) as u32;
        if digits == 0 {
            return None;
        }
        micros = if digits <= 6 {
            value * 10_i128.pow(6 - digits)
        } else {
            value / 10_i128.pow(digits - 6)
        };
    }
    let mut offset_seconds: i128 = 0;
    match bytes.get(at) {
        None | Some(b'Z') | Some(b'z') => {}
        Some(sign @ (b'+' | b'-')) => {
            let direction: i128 = if *sign == b'+' { 1 } else { -1 };
            at += 1;
            let (off_hour, next) = take_digits(bytes, at, 2)?;
            at = next;
            let mut off_minute: u32 = 0;
            if bytes.get(at) == Some(&b':') {
                at += 1;
            }
            if let Some((parsed, _)) = take_digits(bytes, at, 2) {
                off_minute = parsed;
            }
            offset_seconds = direction * (off_hour as i128 * 3600 + off_minute as i128 * 60);
        }
        Some(_) => return None,
    }
    let local = days * 86_400
        + hour as i128 * 3600
        + minute as i128 * 60
        + second as i128;
    Some(local * 1_000_000 + micros - offset_seconds * 1_000_000)
}

/// Whether a wait stands expired at an instant, under obligation
/// SY-22. A wait is expired where the operation instant stands
/// at or past the recorded instant plus its bound, and a wait
/// with no bound never expires.
fn wait_expired(wait: &Wait, instant: &str) -> bool {
    let Some(bound) = wait.wait_for else {
        return false;
    };
    let (Some(recorded), Some(now)) = (stamp_micros(&wait.recorded_at), stamp_micros(instant))
    else {
        return false;
    };
    now >= recorded + bound as i128 * 1_000_000
}

/// Remove the waits that stand expired at an instant from a
/// loaded registry, returning them in the order they were
/// recorded. The removal takes effect only in the publish of the
/// operation that swept, so a refusing operation publishes
/// nothing and the expired waits stay recorded.
fn sweep_expired(data: &mut Registry, instant: &str) -> Vec<Wait> {
    let (standing, expired): (Vec<Wait>, Vec<Wait>) = std::mem::take(&mut data.waits)
        .into_iter()
        .partition(|wait| !wait_expired(wait, instant));
    data.waits = standing;
    expired
}

fn take_digits(bytes: &[u8], at: usize, count: usize) -> Option<(u32, usize)> {
    if at + count > bytes.len() {
        return None;
    }
    let mut value: u32 = 0;
    for offset in 0..count {
        let byte = bytes[at + offset];
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as u32;
    }
    Some((value, at + count))
}

/// Whether a claimed_at value parses as a timestamp. The accepted
/// grammar is the ISO 8601 shape the implementation itself writes,
/// an extended or basic format date, one separator character, a
/// time with an optional fraction, and an optional offset, with the
/// field ranges checked, which is the shape the registry format
/// carries.
pub fn is_valid_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let (year, mut at) = match take_digits(bytes, 0, 4) {
        Some(pair) => pair,
        None => return false,
    };
    let (month, day);
    if at < bytes.len() && bytes[at] == b'-' {
        let (value, next) = match take_digits(bytes, at + 1, 2) {
            Some(pair) => pair,
            None => return false,
        };
        month = value;
        if next >= bytes.len() || bytes[next] != b'-' {
            return false;
        }
        let (value, next) = match take_digits(bytes, next + 1, 2) {
            Some(pair) => pair,
            None => return false,
        };
        day = value;
        at = next;
    } else {
        let (value, next) = match take_digits(bytes, at, 2) {
            Some(pair) => pair,
            None => return false,
        };
        month = value;
        let (value, next) = match take_digits(bytes, next, 2) {
            Some(pair) => pair,
            None => return false,
        };
        day = value;
        at = next;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year as i64, month) {
        return false;
    }
    if at == bytes.len() {
        return true;
    }
    at += 1;
    let (hour, next) = match take_digits(bytes, at, 2) {
        Some(pair) => pair,
        None => return false,
    };
    at = next;
    if hour > 23 {
        return false;
    }
    if at == bytes.len() {
        return true;
    }
    if bytes[at] != b':' {
        return false;
    }
    let (minute, next) = match take_digits(bytes, at + 1, 2) {
        Some(pair) => pair,
        None => return false,
    };
    at = next;
    let mut second = 0;
    if at < bytes.len() && bytes[at] == b':' {
        let (value, next) = match take_digits(bytes, at + 1, 2) {
            Some(pair) => pair,
            None => return false,
        };
        second = value;
        at = next;
    }
    if minute > 59 || second > 59 {
        return false;
    }
    if at < bytes.len() && bytes[at] == b'.' {
        let start = at + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return false;
        }
        at = end;
    }
    if at == bytes.len() {
        return true;
    }
    if bytes[at] == b'Z' {
        return at + 1 == bytes.len();
    }
    if bytes[at] == b'+' || bytes[at] == b'-' {
        let rest = &bytes[at + 1..];
        let digits_only = |part: &[u8]| part.iter().all(|byte| byte.is_ascii_digit());
        let shape = match rest.len() {
            2 | 4 | 6 => digits_only(rest),
            5 => rest[2] == b':' && digits_only(&rest[..2]) && digits_only(&rest[3..]),
            8 => {
                rest[2] == b':'
                    && rest[5] == b':'
                    && digits_only(&rest[..2])
                    && digits_only(&rest[3..5])
                    && digits_only(&rest[6..])
            }
            _ => false,
        };
        if !shape {
            return false;
        }
        let offset_hour = (rest[0] - b'0') as u32 * 10 + (rest[1] - b'0') as u32;
        return offset_hour <= 23;
    }
    false
}

pub const GIT_LOCATION_VARIABLES: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

pub const GIT_CONFIG_VARIABLES: [&str; 6] = [
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

fn is_git_config_pair(name: &str) -> bool {
    for prefix in ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit());
        }
    }
    false
}

/// The environment handed to git, with caller steering removed.
///
/// A location variable inherited from the caller, GIT_DIR above
/// all, would retarget every git call at a foreign repository even
/// though this helper names its repository with -C, so the location
/// variables never pass through. The config variables and the
/// numbered config key and value pairs would let the caller steer
/// what git reads as its configuration, so they never pass through
/// either.
pub fn git_env_from<I: IntoIterator<Item = (String, String)>>(vars: I) -> Vec<(String, String)> {
    vars.into_iter()
        .filter(|(name, _)| {
            !GIT_LOCATION_VARIABLES.contains(&name.as_str())
                && !GIT_CONFIG_VARIABLES.contains(&name.as_str())
                && !is_git_config_pair(name)
        })
        .collect()
}

pub fn git_env() -> Vec<(String, String)> {
    git_env_from(std::env::vars_os().map(|(key, value)| {
        (
            key.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }))
}

/// The captured outcome of one git call.
#[derive(Debug)]
pub struct GitOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The seam the git calls run through, so the crate's own tests can
/// stage a git failure no real repository offers.
pub trait GitRunner {
    fn result(&self, repo: &Path, args: &[&str]) -> Result<GitOutput>;
}

/// The real git runner, a subprocess under a timeout.
pub struct ProcessGit;

impl GitRunner for ProcessGit {
    fn result(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
        run_git(
            repo,
            args,
            &git_env(),
            Duration::from_secs(GIT_TIMEOUT_SECONDS),
        )
    }
}

fn run_git(
    repo: &Path,
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<GitOutput> {
    let joined = args.join(" ");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|exc| Error::plain(format!("git {joined} could not be started, {exc}")))?;
    let mut out_pipe = child.stdout.take().unwrap();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let mut err_pipe = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The reader threads end detached on this
                    // path. A grandchild of git can hold the
                    // pipes open after git itself is dead, and
                    // joining here would block past the timeout
                    // this call was promised to keep.
                    drop(out_thread);
                    drop(err_thread);
                    return Err(Error::plain(format!(
                        "git {joined} did not finish within {} seconds and was stopped",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(exc) => {
                return Err(Error::plain(format!(
                    "git {joined} could not be waited on, {exc}"
                )));
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(GitOutput {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

fn git_detail(result: &GitOutput) -> String {
    let stderr = result.stderr.trim();
    if stderr.is_empty() {
        result.stdout.trim().to_string()
    } else {
        stderr.to_string()
    }
}

fn git_checked(git: &dyn GitRunner, repo: &Path, args: &[&str]) -> Result<String> {
    let result = git.result(repo, args)?;
    if result.code != 0 {
        return Err(Error::plain(format!(
            "git {} failed with exit {}, {}",
            args.join(" "),
            result.code,
            git_detail(&result)
        )));
    }
    Ok(result.stdout)
}

/// Refuse to run git unless the stack repo is its own repository.
///
/// Git finds a repository by walking upward, so a stack repo that
/// lost its .git entry while sitting inside a larger repository
/// would make every git operation land silently in the ancestor. A
/// stack repo that is a linked worktree of a foreign repository
/// passes that test, because its top level is itself, while every
/// git operation still lands in the foreign repository that owns
/// the common git directory. Claim and release call this before
/// they run any git command, and the common git directory must
/// resolve to exactly the stack repo's own .git directory.
fn verify_own_repo(git: &dyn GitRunner, stack: &Path) -> Result<()> {
    let repo = stack.join(REPO_NAME);
    if !repo.join(".git").exists() {
        return Err(Error::plain(format!(
            "the stack repository at {} has no .git entry of its own, \
             so switchyard will not run git against it",
            repo.display()
        )));
    }
    let result = git.result(&repo, &["rev-parse", "--show-toplevel"])?;
    if result.code != 0 {
        return Err(Error::plain(format!(
            "the stack repository at {} cannot be read by git, {}",
            repo.display(),
            git_detail(&result)
        )));
    }
    let top = result.stdout.trim();
    if resolve_path(Path::new(top)) != resolve_path(&repo) {
        return Err(Error::plain(format!(
            "the stack repository at {} resolves to the repository at {top}, \
             which is not the stack repository, \
             so switchyard will not run git against it",
            repo.display()
        )));
    }
    let common = git.result(&repo, &["rev-parse", "--git-common-dir"])?;
    if common.code != 0 {
        return Err(Error::plain(format!(
            "the stack repository at {} cannot be read by git, {}",
            repo.display(),
            git_detail(&common)
        )));
    }
    let printed = PathBuf::from(common.stdout.trim());
    let common_dir = if printed.is_absolute() {
        printed
    } else {
        repo.join(printed)
    };
    if resolve_path(&common_dir) != resolve_path(&repo.join(".git")) {
        return Err(Error::plain(format!(
            "the stack repository at {} shares the git directory at {}, \
             which is not its own .git directory, \
             so switchyard will not run git against it",
            repo.display(),
            resolve_path(&common_dir).display()
        )));
    }
    Ok(())
}

fn remove_created(paths: &[PathBuf]) -> Vec<String> {
    let mut problems = Vec::new();
    for path in paths {
        let removed = match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(_) => Ok(()),
        };
        if let Err(exc) = removed {
            problems.push(format!("{} could not be removed, {exc}", path.display()));
        }
    }
    problems
}

/// Create a stack root with a repository, a lock file, and a
/// registry.
///
/// Init is failure atomic in effect. If any step fails, everything
/// init created is removed again and the failure is reported as one
/// clean error, so no half initialised stack is left for later
/// commands to misread.
pub fn init_stack(
    stack: &Path,
    port_start: i128,
    range_size: i128,
    range_count: i128,
) -> Result<Pool> {
    init_stack_with(stack, port_start, range_size, range_count, &ProcessGit)
}

fn init_stack_with(
    stack: &Path,
    port_start: i128,
    range_size: i128,
    range_count: i128,
    git: &dyn GitRunner,
) -> Result<Pool> {
    let stack = resolve_path(stack);
    if stack.join(REGISTRY_NAME).is_file() {
        return Err(Error::plain(format!(
            "stack at {} is already initialised",
            stack.display()
        )));
    }
    for (value, label) in [
        (port_start, "port start"),
        (range_size, "range size"),
        (range_count, "range count"),
    ] {
        if value <= 0 {
            return Err(Error::plain(format!(
                "the {label} must be a positive integer"
            )));
        }
    }
    match pool_end_wide(port_start, range_size, range_count) {
        Some(end) if end <= 65535 => {}
        _ => return Err(Error::plain("the port pool reaches past port 65535")),
    }
    // Every pool number is positive and the pool ends at or before
    // port 65535, so each number fits the recorded width exactly.
    let pool = Pool {
        start: port_start as i64,
        range_size: range_size as i64,
        range_count: range_count as i64,
    };
    let repo = stack.join(REPO_NAME);
    let worktrees = stack.join(WORKTREES_NAME);
    let lock_path = stack.join(LOCK_NAME);
    let registry = stack.join(REGISTRY_NAME);
    if repo.exists() || repo.symlink_metadata().is_ok() {
        return Err(Error::plain(format!(
            "init refuses the stack at {} because {} already exists, \
             and the stack root must not already hold a repo path \
             that init did not create, so nothing was changed",
            stack.display(),
            repo.display()
        )));
    }
    if (worktrees.exists() || worktrees.symlink_metadata().is_ok()) && !worktrees.is_dir() {
        return Err(Error::plain(format!(
            "init refuses the stack at {} because {} already exists \
             and is not a directory, so nothing was changed",
            stack.display(),
            worktrees.display()
        )));
    }
    let watched: Vec<PathBuf> = vec![
        registry,
        lock_path.clone(),
        repo.clone(),
        worktrees.clone(),
        stack.clone(),
    ];
    let preexisting: Vec<bool> = watched
        .iter()
        .map(|path| path.exists() || path.symlink_metadata().is_ok())
        .collect();
    let attempt = (|| -> Result<Pool> {
        fs::create_dir_all(&worktrees).map_err(|exc| Error::Io(exc.to_string()))?;
        fs::create_dir_all(&repo).map_err(|exc| Error::Io(exc.to_string()))?;
        git_checked(
            git,
            &stack,
            &["init", "-q", "-b", "main", &repo.to_string_lossy()],
        )?;
        git_checked(
            git,
            &repo,
            &["config", "user.email", "switchyard@example.invalid"],
        )?;
        git_checked(git, &repo, &["config", "user.name", "switchyard"])?;
        fs::write(
            repo.join("README.md"),
            "Synthetic stack repository created by switchyard init.\n",
        )
        .map_err(|exc| Error::Io(exc.to_string()))?;
        git_checked(git, &repo, &["add", "README.md"])?;
        git_checked(
            git,
            &repo,
            &["commit", "-q", "-m", "Initialise the stack repository"],
        )?;
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|exc| Error::Io(exc.to_string()))?;
        let data = Registry {
            version: REGISTRY_VERSION,
            port_pool: pool,
            claims: BTreeMap::new(),
            waits: Vec::new(),
        };
        save_registry(&stack, &data)?;
        Ok(pool)
    })();
    match attempt {
        Ok(pool) => Ok(pool),
        Err(exc) => {
            let created: Vec<PathBuf> = watched
                .iter()
                .zip(preexisting.iter())
                .filter(|(_, already)| !**already)
                .map(|(path, _)| path.clone())
                .collect();
            let problems = remove_created(&created);
            if !problems.is_empty() {
                return Err(Error::plain(format!(
                    "init of the stack at {} failed, {}, and the partial stack \
                     could not be fully removed, {}",
                    stack.display(),
                    exc.message(),
                    problems.join(", ")
                )));
            }
            match exc {
                Error::Io(detail) => Err(Error::plain(format!(
                    "the stack at {} cannot be created, {detail}",
                    stack.display()
                ))),
                other => Err(other),
            }
        }
    }
}

/// The first run of consecutive free ranges of the given length,
/// as its first port and its last port. A draw is made whole or
/// not at all, so a pool where single ranges stand free but no run
/// of the needed length stands free furnishes nothing here.
fn free_run(data: &Registry, ranges: i64) -> Option<(i64, i64)> {
    let pool = &data.port_pool;
    if ranges < 1 || ranges > pool.range_count {
        return None;
    }
    let taken: Vec<(i64, i64)> = data
        .claims
        .values()
        .map(|record| (record.port_start, record.port_end))
        .collect();
    for index in 0..=(pool.range_count - ranges) {
        let start = pool.start + index * pool.range_size;
        let end = start + ranges * pool.range_size - 1;
        if taken.iter().all(|(low, high)| end < *low || start > *high) {
            return Some((start, end));
        }
    }
    None
}

/// The lengths of the free runs standing in the pool, in pool
/// order, as the count of ranges each maximal run of untenanted
/// ranges holds. The scoring vignette of obligation SY-20 states
/// these lengths and nothing else about the pool's shape.
fn free_runs(data: &Registry) -> Vec<i64> {
    let pool = &data.port_pool;
    let mut taken: BTreeSet<i64> = BTreeSet::new();
    for record in data.claims.values() {
        let first = (record.port_start - pool.start) / pool.range_size;
        let last = (record.port_end - pool.start) / pool.range_size;
        for index in first..=last {
            taken.insert(index);
        }
    }
    let mut runs: Vec<i64> = Vec::new();
    let mut current: i64 = 0;
    for index in 0..pool.range_count {
        if taken.contains(&index) {
            if current > 0 {
                runs.push(current);
                current = 0;
            }
        } else {
            current += 1;
        }
    }
    if current > 0 {
        runs.push(current);
    }
    runs
}

fn env_text(record: &Record) -> String {
    let lines = [
        "# Written by switchyard. This file dies with the worktree.".to_string(),
        format!("SWITCHYARD_AGENT={}", record.agent),
        format!("SWITCHYARD_BRANCH={}", record.branch),
        format!("SWITCHYARD_WORKTREE={}", record.worktree),
        format!("SWITCHYARD_PORT_START={}", record.port_start),
        format!("SWITCHYARD_PORT_END={}", record.port_end),
        format!("PORT={}", record.port_start),
        format!("SWITCHYARD_DATABASE={}", record.database),
        format!(
            "DATABASE_URL=postgres://127.0.0.1:{}/{}",
            record.port_start, record.database
        ),
    ];
    lines.join("\n") + "\n"
}

fn write_env_file(stack: &Path, env_rel: &str, record: &Record) -> Result<()> {
    let path = stack.join(env_rel);
    fs::write(&path, env_text(record)).map_err(|exc| {
        Error::plain(format!(
            "the env file at {} cannot be written, {exc}",
            path.display()
        ))
    })
}

fn require_committed_head(git: &dyn GitRunner, repo: &Path) -> Result<()> {
    let result = git.result(repo, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    if result.code != 0 {
        return Err(Error::plain(format!(
            "the stack repository at {} has no commits yet, \
             so no claim can branch from it",
            repo.display()
        )));
    }
    Ok(())
}

/// Best effort removal of whatever a failed claim created.
///
/// The branch is deleted whether or not the worktree add reported
/// success, because git creates the branch inside the add command
/// itself, so a failed add can still leave a branch behind that
/// would block every later claim for the agent. It is deleted only
/// when this claim attempt created it. A branch that already stood
/// in the repository before the attempt is not the claim's to
/// remove, and it is left standing with whatever commits it
/// carries. The worktree is removed only when it carries the .git
/// file of a real worktree, so a directory that was already in the
/// way is left untouched. Returns a list of problem descriptions,
/// empty when everything was removed or never existed.
pub fn rollback_claim(
    git: &dyn GitRunner,
    repo: &Path,
    worktree: &Path,
    branch: &str,
    branch_preexisted: bool,
) -> Vec<String> {
    let mut problems = Vec::new();
    if worktree.join(".git").exists() {
        match git.result(
            repo,
            &["worktree", "remove", "--force", &worktree.to_string_lossy()],
        ) {
            Ok(result) if result.code == 0 => {}
            Ok(result) => problems.push(format!(
                "the worktree at {} could not be removed, {}",
                worktree.display(),
                git_detail(&result)
            )),
            Err(exc) => problems.push(format!(
                "the worktree at {} could not be removed, {}",
                worktree.display(),
                exc.message()
            )),
        }
    }
    if !branch_preexisted {
        if let Ok(listed) = git.result(repo, &["branch", "--list", branch]) {
            if listed.code == 0 && !listed.stdout.trim().is_empty() {
                match git.result(repo, &["branch", "-D", branch]) {
                    Ok(result) if result.code == 0 => {}
                    Ok(result) => problems.push(format!(
                        "the branch {branch} could not be deleted, {}",
                        git_detail(&result)
                    )),
                    Err(exc) => problems.push(format!(
                        "the branch {branch} could not be deleted, {}",
                        exc.message()
                    )),
                }
            }
        }
    }
    problems
}

/// Allocate a worktree, a branch, a port range, a database, an env
/// file.
///
/// The whole check and set runs under the stack lock, and the
/// registry is loaded only after the lock is held, so a racing
/// process can never act on a stale read.
pub fn claim(stack: &Path, agent: &str) -> Result<Record> {
    claim_with(stack, agent, &ProcessGit, &save_registry)
}

/// The opening every claim shape shares once the stack stands
/// resolved and the agent name checked, in the landed order. The
/// lock is taken, the registry loaded under it, and the stack
/// repository verified. The standing claim check stands apart
/// because the third phase of a routed claim records that refusal
/// before it returns it, where the other shapes return it bare.
fn claim_prologue(stack: &Path, git: &dyn GitRunner) -> Result<(Locked, Registry)> {
    let lock = locked(stack)?;
    let data = load_registry(stack)?;
    verify_own_repo(git, stack)?;
    Ok((lock, data))
}

/// The refusal of a claim whose agent already holds a claim in
/// the stack, in the one wording every claim shape returns it in.
fn standing_claim_error(agent: &str) -> Error {
    Error::plain(format!("agent {agent} already holds a claim in this stack"))
}

/// The standing position check of obligation SY-19, where an
/// agent holding a claim or a wait refuses as duplicate under
/// the one refusal and the one wording. Every claim shape takes
/// this check, because an agent standing in both the claims and
/// the waits of one document is a shape the reader refuses as
/// corrupt, and no claim shape may publish it.
fn refuse_standing_position(data: &Registry, agent: &str) -> Result<()> {
    if data.claims.contains_key(agent) || data.waits.iter().any(|wait| wait.agent == agent) {
        return Err(standing_claim_error(agent));
    }
    Ok(())
}

/// Refuse a wait bound that is not a positive number of seconds,
/// before anything is read or recorded, in every waiting entry
/// point, under obligation SY-17.
fn validate_wait_bound(wait_for: Option<i64>) -> Result<()> {
    if let Some(bound) = wait_for {
        if bound <= 0 {
            return Err(Error::plain(format!(
                "a wait bound must be a positive number of seconds, not {bound}"
            )));
        }
    }
    Ok(())
}

/// The queue full refusal of obligation SY-17, naming the
/// bound, in the one wording every claim shape returns it in.
fn queue_full_error() -> Error {
    Error::plain(format!(
        "the wait queue is full, at most {QUEUE_BOUND} waits may stand in one stack"
    ))
}

/// Refuse a waiting claim where the queue already stands at the
/// bound of obligation SY-17, with a message naming the bound.
fn refuse_queue_full(data: &Registry) -> Result<()> {
    if data.waits.len() >= QUEUE_BOUND {
        return Err(queue_full_error());
    }
    Ok(())
}

fn claim_with(
    stack: &Path,
    agent: &str,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<Record> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let (lock, mut data) = claim_prologue(&stack, git)?;
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    refuse_standing_position(&data, agent)?;
    allocate_locked(AllocParts {
        stack: &stack,
        agent,
        draw: 1,
        data,
        lock: &lock,
        git,
        saver,
        routing: None,
        expired,
        instant,
    })
}

/// The inputs of the shared allocation attempt, bundled because
/// the attempt is the one path every claim shape takes and its
/// inputs name one another badly as a bare argument list.
struct AllocParts<'a> {
    stack: &'a Path,
    agent: &'a str,
    draw: i64,
    data: Registry,
    lock: &'a Locked,
    git: &'a dyn GitRunner,
    saver: &'a dyn Fn(&Path, &Registry) -> Result<()>,
    routing: Option<&'a RoutingLine>,
    /// The waits the caller's sweep removed from the loaded
    /// registry, whose leavings the queue log records in the same
    /// act as this allocation's publish, under obligation SY-22.
    expired: Vec<Wait>,
    /// The one instant of the locked act, which the claim record
    /// carries as its claimed_at.
    instant: String,
}

/// One furnished claim built inside a caller's registry, with
/// the side effects of the landed allocation attempt standing
/// done and the claim standing in the caller's map, for the
/// caller to publish. The branch preexistence flag rides along
/// because a rollback needs it.
struct Furnished {
    record: Record,
    branch_preexisted: bool,
}

/// Roll a failed furnishing back, returning the bare error where
/// the rollback completed and the landed combined message where
/// it did not.
fn rollback_or_report(
    git: &dyn GitRunner,
    repo: &Path,
    record: &Record,
    branch_preexisted: bool,
    exc: Error,
) -> Error {
    let worktree = repo
        .parent()
        .map(|stack| stack.join(&record.worktree))
        .unwrap_or_default();
    let problems = rollback_claim(git, repo, &worktree, &record.branch, branch_preexisted);
    if problems.is_empty() {
        exc
    } else {
        Error::plain(format!(
            "the claim for {} failed, {}, and rolling it back was incomplete, {}",
            record.agent,
            exc.message(),
            problems.join(", ")
        ))
    }
}

/// Furnish one claim inside a loaded registry whose lock the
/// caller holds, performing the landed allocation side effects
/// without publishing. The pool refusals are the landed
/// messages, and the record carries the instant the caller
/// names, which is the instant of the locked act the claim
/// stands in. A failure rolls the furnishing itself back and
/// returns the error, and the caller publishes nothing.
fn furnish_claim(
    data: &mut Registry,
    stack: &Path,
    agent: &str,
    draw: i64,
    claimed_at: &str,
    git: &dyn GitRunner,
) -> Result<Furnished> {
    let repo = stack.join(REPO_NAME);
    let branch = branch_name(agent);
    let worktree_rel = format!("{WORKTREES_NAME}/{agent}");
    let worktree = stack.join(&worktree_rel);
    let env_rel = format!("{worktree_rel}/{ENV_NAME}");
    let (port_start, port_end) = match free_run(data, draw) {
        Some(range) => range,
        None if draw == 1 => {
            return Err(Error::plain(
                "the port pool is exhausted, every range is held",
            ))
        }
        None => {
            return Err(Error::plain(format!(
                "the port pool cannot furnish a draw of {draw} consecutive ranges"
            )))
        }
    };
    let record = Record {
        agent: agent.to_string(),
        branch: branch.clone(),
        worktree: worktree_rel,
        port_start,
        port_end,
        database: database_name(agent),
        env_file: env_rel.clone(),
        claimed_at: claimed_at.to_string(),
    };
    let listed = git.result(&repo, &["branch", "--list", &branch])?;
    let branch_preexisted = listed.code == 0 && !listed.stdout.trim().is_empty();
    let attempt = (|| -> Result<()> {
        require_committed_head(git, &repo)?;
        git_checked(
            git,
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree.to_string_lossy(),
            ],
        )?;
        git_checked(
            git,
            &repo,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )?;
        write_env_file(stack, &env_rel, &record)?;
        data.claims.insert(agent.to_string(), record.clone());
        Ok(())
    })();
    match attempt {
        Ok(()) => Ok(Furnished {
            record,
            branch_preexisted,
        }),
        Err(exc) => Err(rollback_or_report(git, &repo, &record, branch_preexisted, exc)),
    }
}

/// The shared allocation attempt, run with the stack lock held and
/// the registry loaded and the standing claim check already made.
///
/// A draw of one range is the claim Round 1 landed. A larger draw
/// takes that many consecutive ranges for a profiled or routed
/// claim, and the record carries the start of the first range it
/// drew and the end of the last in the landed record shape. When a
/// routing line stands, it is appended to the routing log after
/// the allocation is built and immediately before the registry
/// publish, as obligation SY-15 fixes the order, and the queue
/// lines of the caller's sweep stand before the routing line in
/// the same act, under obligation SY-23.
fn allocate_locked(parts: AllocParts) -> Result<Record> {
    let AllocParts {
        stack,
        agent,
        draw,
        mut data,
        lock,
        git,
        saver,
        routing,
        expired,
        instant,
    } = parts;
    let furnished = furnish_claim(&mut data, stack, agent, draw, &instant, git)?;
    let attempt = (|| -> Result<()> {
        lock.verify_current()?;
        append_leavings(stack, &instant, "expired", &expired, None)?;
        if let Some(line) = routing {
            line.append(stack)?;
        }
        saver(stack, &data)?;
        Ok(())
    })();
    match attempt {
        Ok(()) => Ok(furnished.record),
        Err(exc) => Err(rollback_or_report(
            git,
            &stack.join(REPO_NAME),
            &furnished.record,
            furnished.branch_preexisted,
            exc,
        )),
    }
}

/// What a release did, of obligation SY-21 and SY-22. A release
/// of an agent holding a claim returns the claim it released, and
/// a release of an agent holding only a wait cancels that wait
/// and returns it.
#[derive(Debug, Clone, PartialEq)]
pub enum ReleaseOutcome {
    Claim(Record),
    Wait(Wait),
}

/// Serve the standing waits of a release whose claim's removal
/// already stands computed in the registry, in offer order, under
/// obligation SY-21. A wait whose whole draw stands free at its
/// turn is furnished whole through the landed allocation path of
/// its kind, its claim record carrying the instant of the
/// release, and a wait whose draw does not stand free is skipped
/// and does not block the waits behind it. Returns the waits
/// furnished, in offer order, with the span each furnished claim
/// took, and the registry carries every furnishing at once, for
/// the caller's one publish.
fn serve_offers(
    data: &mut Registry,
    stack: &Path,
    instant: &str,
    git: &dyn GitRunner,
) -> Result<(Vec<Wait>, BTreeMap<String, (i64, i64)>)> {
    let mut furnished: Vec<Wait> = Vec::new();
    let mut spans: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for wait in offer_order(&data.waits) {
        let draw = settled_draw(&wait.profile);
        if free_run(data, draw).is_none() {
            continue;
        }
        let done = furnish_claim(data, stack, &wait.agent, draw, instant, git)?;
        spans.insert(
            wait.agent.clone(),
            (done.record.port_start, done.record.port_end),
        );
        furnished.push(wait);
    }
    if !furnished.is_empty() {
        let done: BTreeSet<&str> = furnished.iter().map(|wait| wait.agent.as_str()).collect();
        data.waits.retain(|wait| !done.contains(wait.agent.as_str()));
    }
    Ok((furnished, spans))
}

/// Return every resource a claim holds and remove its record, or
/// cancel the wait of an agent who holds only a wait, under
/// obligations SY-21 and SY-22. The sweep of expired waits stands
/// at the head of the operation, after validation and before any
/// other queue decision, and the release and the sweep and every
/// offer the release makes publish in the one rename.
pub fn release(stack: &Path, agent: &str) -> Result<ReleaseOutcome> {
    release_with(stack, agent, &ProcessGit)
}

fn release_with(stack: &Path, agent: &str, git: &dyn GitRunner) -> Result<ReleaseOutcome> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let repo = stack.join(REPO_NAME);
    let lock = locked(&stack)?;
    let mut data = load_registry(&stack)?;
    verify_own_repo(git, &stack)?;
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    if let Some(record) = data.claims.get(agent).cloned() {
        let worktree = stack.join(&record.worktree);
        if worktree.exists() {
            git_checked(
                git,
                &repo,
                &["worktree", "remove", "--force", &worktree.to_string_lossy()],
            )?;
        } else {
            git_checked(git, &repo, &["worktree", "prune"])?;
        }
        let listed = git_checked(git, &repo, &["branch", "--list", &record.branch])?;
        if !listed.trim().is_empty() {
            git_checked(git, &repo, &["branch", "-D", &record.branch])?;
        }
        data.claims.remove(agent);
        let (furnished, spans) = serve_offers(&mut data, &stack, &instant, git)?;
        append_leavings(&stack, &instant, "expired", &expired, None)?;
        append_leavings(&stack, &instant, "furnished", &furnished, Some(&spans))?;
        lock.verify_current()?;
        save_registry(&stack, &data)?;
        return Ok(ReleaseOutcome::Claim(record));
    }
    if let Some(wait) = data.waits.iter().find(|wait| wait.agent == agent).cloned() {
        data.waits.retain(|standing| standing.agent != agent);
        append_leavings(&stack, &instant, "expired", &expired, None)?;
        append_leavings(&stack, &instant, "cancelled", &[wait.clone()], None)?;
        lock.verify_current()?;
        save_registry(&stack, &data)?;
        return Ok(ReleaseOutcome::Wait(wait));
    }
    Err(Error::plain(format!(
        "agent {agent} holds no claim in this stack"
    )))
}

/// The standing waits in offer order, derived only from the
/// registry, under obligation SY-24. The list takes no lock and
/// sweeps nothing, because expiry is evaluated on access by the
/// operations that publish, and a reader changes nothing.
pub fn list_waits(stack: &Path) -> Result<Vec<Wait>> {
    let stack = resolve_path(stack);
    let data = load_registry(&stack)?;
    Ok(offer_order(&data.waits))
}

/// The offer order of obligation SY-21. Scored waits stand
/// before scoreless waits, scored waits order by score
/// descending with ties by arrival, and scoreless waits order by
/// arrival, where arrival is the order of the recorded list.
pub fn offer_order(waits: &[Wait]) -> Vec<Wait> {
    let mut scored: Vec<Wait> = waits.iter().filter(|wait| wait.score.is_some()).cloned().collect();
    scored.sort_by(|left, right| right.score.cmp(&left.score));
    let scoreless: Vec<Wait> = waits.iter().filter(|wait| wait.score.is_none()).cloned().collect();
    scored.extend(scoreless);
    scored
}

/// Return the recorded claims sorted by agent, reading nothing
/// else.
pub fn list_claims(stack: &Path) -> Result<Vec<Record>> {
    let stack = resolve_path(stack);
    let data = load_registry(&stack)?;
    Ok(data.claims.values().cloned().collect())
}

/// One declared allocation profile of obligation SY-12.
///
/// The profiles are constants of the library and of the contract,
/// declared once for every stack. They live in neither registry
/// state nor claim parameters, because the choice set is the
/// doctrine the routing prompt states, and a judge whose choices
/// change from stack to stack or from caller to caller is a judge
/// applying a doctrine nobody recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Lean,
    Standard,
    Roomy,
}

impl Profile {
    pub fn name(&self) -> &'static str {
        match self {
            Profile::Lean => "lean",
            Profile::Standard => "standard",
            Profile::Roomy => "roomy",
        }
    }

    /// The draw of the profile, in whole ranges of the recorded
    /// pool. A plain claim draws one range, as the lean profile
    /// does, while remaining different in kind, because a plain
    /// claim names no profile and none is inferred for it.
    pub fn draw(&self) -> i64 {
        match self {
            Profile::Lean => 1,
            Profile::Standard => 2,
            Profile::Roomy => 4,
        }
    }

    pub fn from_name(name: &str) -> Option<Profile> {
        PROFILES
            .iter()
            .find(|profile| profile.name() == name)
            .copied()
    }
}

/// The declared profiles, in the order the contract names them.
pub const PROFILES: [Profile; 3] = [Profile::Lean, Profile::Standard, Profile::Roomy];

/// Resolve a caller supplied profile name to a declared profile,
/// refusing a name outside the declared set the way the init
/// command refuses a pool number that is not positive.
pub fn profile_named(name: &str) -> Result<Profile> {
    Profile::from_name(name).ok_or_else(|| {
        Error::plain(format!(
            "unknown profile {name}, the declared profiles are lean and standard and roomy"
        ))
    })
}

/// The seam a model is consulted through, moving text only.
///
/// A transport receives the exact request document the library
/// built and returns the reply body as text, and the library alone
/// classifies that text under the grammar of obligation SY-16, so
/// a recorded reply and a live reply and a broken reply are all
/// judged by the one reader. The error text is the detail of the
/// failure, which the classification records only as the transport
/// class.
pub trait JudgeTransport {
    fn consult(&self, request: &str) -> std::result::Result<String, String>;
}

/// A transport that answers from recorded replies, keyed by the
/// SHA-256 digest of the exact request document.
///
/// A request with no recorded reply is a transport failure of its
/// own recorded kind, so a test that drifts from its fixtures
/// fails as a named transport event and never as a silent landing
/// on a default it did not pin.
pub struct FixtureTransport {
    replies: std::collections::BTreeMap<String, String>,
}

impl FixtureTransport {
    pub fn new(replies: std::collections::BTreeMap<String, String>) -> FixtureTransport {
        FixtureTransport { replies }
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> FixtureTransport {
        FixtureTransport {
            replies: pairs.into_iter().collect(),
        }
    }
}

impl JudgeTransport for FixtureTransport {
    fn consult(&self, request: &str) -> std::result::Result<String, String> {
        let digest = sha256_hex(request.as_bytes());
        self.replies
            .get(&digest)
            .cloned()
            .ok_or_else(|| format!("no recorded reply for the request digest {digest}"))
    }
}

/// The SHA-256 digest of a byte string, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The counting doctrine the routing question carries, stated in
/// so many words because a judge applies the doctrine the prompt
/// states and does not supply one left implicit.
const ROUTING_INSTRUCTIONS: &str = "The question asks the judge to choose the allocation profile for one job from its description. Count only the processes the job itself runs at the same time that each need a port of their own. A process that runs but binds no port does not count. Duration is not footprint, and urgency is not footprint, and data volume is not footprint. A database the job connects to does not count, and neither does any service the job merely talks to. Work that other agents claim for themselves does not count. Enumerations govern, so copies and shards and tenants and replicas and auxiliaries each count when each needs its own port. The lean profile is for a count of at most two. The standard profile is for a count of three to twelve. The roomy profile is for a count of more than twelve. Where the description does not determine the count, the answer is the none of these label, and a guess is a wrong answer. Answer with exactly one of the listed options.";

const ROUTING_FRAMING: &str = "A furnishing tool has been asked to size one claim from a job description.\nThe judge is to apply the counting doctrine of the question to the description below and to nothing else.\nThe answer sizes a port allocation and performs no allocation itself.";

const ROUTING_CLOSING: &str = "The labels are lean and standard and roomy and other. Lean covers a count of at most two, standard covers a count of three to twelve, and roomy covers a count of more than twelve, and other is the answer where the description does not determine the count.";

/// The state text of a routing request, a pure function of the
/// description and of nothing else. No agent name and no stack
/// fact and no pool state enter it, so the same description always
/// builds the same state and the digest in the routing log
/// identifies the judged bytes exactly.
fn routing_state(description: &str) -> String {
    format!("{ROUTING_FRAMING}\n\n{description}\n\n{ROUTING_CLOSING}")
}

/// Build the exact request document a routed claim hands to a
/// transport, as one canonical JSON line naming the pinned model.
pub fn routing_request(description: &str) -> String {
    let mut criteria = serde_json::Map::new();
    criteria.insert(
        "lean".to_string(),
        "The job runs at most two processes at the same time that each need a port of their own."
            .into(),
    );
    criteria.insert(
        "other".to_string(),
        "None of the listed options is the correct decision, because the description leaves the count undetermined."
            .into(),
    );
    criteria.insert(
        "roomy".to_string(),
        "The job runs more than twelve processes at the same time that each need a port of their own."
            .into(),
    );
    criteria.insert(
        "standard".to_string(),
        "The job runs three to twelve processes at the same time that each need a port of their own."
            .into(),
    );
    let mut question = serde_json::Map::new();
    question.insert("criteria".to_string(), criteria.into());
    question.insert("instructions".to_string(), ROUTING_INSTRUCTIONS.into());
    question.insert("type".to_string(), "choice".into());
    let mut questions = serde_json::Map::new();
    questions.insert(ROUTING_QUESTION.to_string(), question.into());
    let mut document = serde_json::Map::new();
    document.insert("model".to_string(), ROUTING_MODEL.into());
    document.insert("questions".to_string(), questions.into());
    document.insert("state".to_string(), routing_state(description).into());
    serde_json::to_string(&serde_json::Value::Object(document)).unwrap()
}

fn jval_f64(value: &JVal) -> Option<f64> {
    match value {
        JVal::Int(number) => Some(*number as f64),
        JVal::UInt(number) => Some(*number as f64),
        JVal::Float(number) if number.is_finite() => Some(*number),
        _ => None,
    }
}

/// What a reply body classifies as under the grammar of SY-16.
enum Consultation {
    Verdict(Profile, f64),
    LowConfidence(Profile, f64),
    Abstention(f64),
    Silence,
    Malformed,
}

/// Classify one reply body. The confidence of a verdict is the
/// probability of the chosen label read from the probabilities
/// map, and a confidence field the reply also carries is never
/// read, because the wire evidence recorded in the Round 3
/// amendment preamble shows it is a separate measurement.
fn classify_reply(body: &str) -> Consultation {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Consultation::Silence;
    }
    let data: JVal = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return Consultation::Malformed,
    };
    if first_duplicate_key(&data).is_some() {
        return Consultation::Malformed;
    }
    let entry = match data
        .get("answers")
        .and_then(|answers| answers.get(ROUTING_QUESTION))
    {
        Some(entry) => entry,
        None => return Consultation::Silence,
    };
    if !matches!(entry, JVal::Obj(_)) {
        return Consultation::Malformed;
    }
    let choice = match entry.get("choice").and_then(JVal::as_str) {
        Some(choice) if VERDICT_LABELS.contains(&choice) => choice.to_string(),
        _ => return Consultation::Malformed,
    };
    let probabilities = match entry.get("probabilities") {
        Some(probabilities @ JVal::Obj(pairs)) if exact_keys(pairs, &VERDICT_LABELS) => {
            probabilities
        }
        _ => return Consultation::Malformed,
    };
    let mut values = [0.0_f64; 4];
    for (slot, label) in values.iter_mut().zip(VERDICT_LABELS.iter()) {
        match probabilities.get(label).and_then(jval_f64) {
            Some(number) if (0.0..=1.0).contains(&number) => *slot = number,
            _ => return Consultation::Malformed,
        }
    }
    let index = VERDICT_LABELS
        .iter()
        .position(|label| *label == choice)
        .unwrap();
    let confidence = values[index];
    if choice == "other" {
        Consultation::Abstention(confidence)
    } else {
        let profile = Profile::from_name(&choice).unwrap();
        if confidence >= LOW_CONFIDENCE_THRESHOLD {
            Consultation::Verdict(profile, confidence)
        } else {
            Consultation::LowConfidence(profile, confidence)
        }
    }
}

/// Refuse a description no consultation may read, an empty or all
/// whitespace text or one past the stated bound, before any
/// consultation and before any allocation.
fn validate_description(description: &str) -> Result<()> {
    if description.trim().is_empty() {
        return Err(Error::plain("the job description is empty"));
    }
    if description.len() > DESCRIPTION_MAX_BYTES {
        return Err(Error::plain(format!(
            "the job description is past the bound of {DESCRIPTION_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

/// The fallback vocabulary the routing log records, of
/// obligations SY-14 and SY-15, typed so the classes the log may
/// carry are checked by the compiler rather than spelled as bare
/// strings at each site that records one. The name of a class is
/// the word the log carries for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    None,
    LowConfidence,
    Abstention,
    Silence,
    Malformed,
    Transport,
    NoTransport,
    NoConsultation,
}

impl Fallback {
    fn name(&self) -> &'static str {
        match self {
            Fallback::None => "none",
            Fallback::LowConfidence => "low confidence",
            Fallback::Abstention => "abstention",
            Fallback::Silence => "silence",
            Fallback::Malformed => "malformed",
            Fallback::Transport => "transport",
            Fallback::NoTransport => "no transport",
            Fallback::NoConsultation => "no consultation",
        }
    }
}

/// One line of the routing log of obligation SY-15, carrying the
/// fields that obligation names and no others.
struct RoutingLine {
    agent: String,
    description: String,
    model: Option<String>,
    verdict: Option<String>,
    confidence: Option<f64>,
    fallback: Fallback,
    allocated: Option<String>,
    refusal: Option<String>,
}

impl RoutingLine {
    /// A line for one routed claim, standing with the fields
    /// every outcome shares and the outcome fields at their null
    /// defaults, for the caller to fill with the outcome it
    /// records.
    fn new(agent: &str, description: &str, model: Option<String>) -> RoutingLine {
        RoutingLine {
            agent: agent.to_string(),
            description: description.to_string(),
            model,
            verdict: None,
            confidence: None,
            fallback: Fallback::None,
            allocated: None,
            refusal: None,
        }
    }

    fn append(&self, stack: &Path) -> Result<()> {
        let text = serde_json::Value::Object(self.fields());
        let path = stack.join(ROUTING_LOG_NAME);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|exc| {
                Error::plain(format!(
                    "the routing log at {} cannot be opened, {exc}",
                    path.display()
                ))
            })?;
        file.write_all(text.to_string().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|exc| {
                Error::plain(format!(
                    "the routing log at {} cannot be written, {exc}",
                    path.display()
                ))
            })
    }

    fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut line = serde_json::Map::new();
        line.insert("agent".to_string(), self.agent.clone().into());
        line.insert(
            "allocated".to_string(),
            match &self.allocated {
                Some(profile) => profile.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert("at".to_string(), now_stamp().into());
        line.insert(
            "confidence".to_string(),
            match self.confidence {
                Some(number) => serde_json::json!(number),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "description_bytes".to_string(),
            (self.description.len() as i64).into(),
        );
        line.insert(
            "description_sha256".to_string(),
            sha256_hex(self.description.as_bytes()).into(),
        );
        line.insert("fallback".to_string(), self.fallback.name().into());
        line.insert(
            "model".to_string(),
            match &self.model {
                Some(model) => model.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "refusal".to_string(),
            match &self.refusal {
                Some(class) => class.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "verdict".to_string(),
            match &self.verdict {
                Some(label) => label.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line
    }
}

/// One line of the queue log of obligation SY-23, carrying the
/// fields that obligation names and no others. The log is
/// evidence, not state, and no command reads it to decide
/// anything.
struct QueueLine {
    at: String,
    kind: &'static str,
    agent: String,
    profile: String,
    request_sha256: Option<String>,
    request_bytes: Option<usize>,
    score: Option<i64>,
    confidence: Option<f64>,
    model: Option<String>,
    fallback: String,
    ports: Option<(i64, i64)>,
}

impl QueueLine {
    /// A line for one wait leaving or joining, carrying the
    /// wait's own recorded fields. A joining line carries the
    /// digest of the scoring request the library built for the
    /// wait, and a leaving line leaves the digest fields to the
    /// caller to recover from the wait's joining line, because
    /// the vignette the request was built from no longer stands
    /// when the wait leaves.
    fn for_wait(at: &str, kind: &'static str, wait: &Wait) -> QueueLine {
        QueueLine {
            at: at.to_string(),
            kind,
            agent: wait.agent.clone(),
            profile: wait.profile.clone(),
            request_sha256: None,
            request_bytes: None,
            score: wait.score,
            confidence: wait.confidence,
            model: wait.model.clone(),
            fallback: wait.fallback.clone(),
            ports: None,
        }
    }

    fn append(&self, stack: &Path) -> Result<()> {
        let text = serde_json::Value::Object(self.fields());
        let path = stack.join(QUEUE_LOG_NAME);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|exc| {
                Error::plain(format!(
                    "the queue log at {} cannot be opened, {exc}",
                    path.display()
                ))
            })?;
        file.write_all(text.to_string().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|exc| {
                Error::plain(format!(
                    "the queue log at {} cannot be written, {exc}",
                    path.display()
                ))
            })
    }

    fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut line = serde_json::Map::new();
        line.insert("agent".to_string(), self.agent.clone().into());
        line.insert("at".to_string(), self.at.clone().into());
        line.insert(
            "confidence".to_string(),
            match self.confidence {
                Some(number) => serde_json::json!(number),
                None => serde_json::Value::Null,
            },
        );
        line.insert("fallback".to_string(), self.fallback.clone().into());
        line.insert("kind".to_string(), self.kind.into());
        line.insert(
            "model".to_string(),
            match &self.model {
                Some(model) => model.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "ports".to_string(),
            match self.ports {
                Some((start, end)) => serde_json::json!([start, end]),
                None => serde_json::Value::Null,
            },
        );
        line.insert("profile".to_string(), self.profile.clone().into());
        line.insert(
            "request_bytes".to_string(),
            match self.request_bytes {
                Some(count) => (count as i64).into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "request_sha256".to_string(),
            match &self.request_sha256 {
                Some(digest) => digest.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        line.insert(
            "score".to_string(),
            match self.score {
                Some(level) => level.into(),
                None => serde_json::Value::Null,
            },
        );
        line
    }
}

/// The digest and byte count each standing wait's joining line
/// recorded in the queue log, keyed by agent, best effort. A
/// leaving line carries the digest of the exact scoring request
/// bytes the library built for the wait, and the registry record
/// carries no digest, so a leaving operation recovers the pair
/// from the wait's own joining line. Reading the log to complete
/// a later evidence line decides nothing, because no allocation
/// and no refusal and no ordering and no expiry ever consults
/// it, and where the log or the joining line does not stand the
/// leaving line carries the digest fields as null rather than a
/// fabricated digest.
fn queued_digests(stack: &Path) -> BTreeMap<String, (String, usize)> {
    let mut digests: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let path = stack.join(QUEUE_LOG_NAME);
    let Ok(raw) = fs::read_to_string(&path) else {
        return digests;
    };
    for text in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        let agent = value.get("agent").and_then(|field| field.as_str());
        match value.get("kind").and_then(|field| field.as_str()) {
            Some("queued") => {
                let digest = value
                    .get("request_sha256")
                    .and_then(|field| field.as_str())
                    .map(str::to_string);
                let bytes = value
                    .get("request_bytes")
                    .and_then(|field| field.as_u64())
                    .map(|count| count as usize);
                if let (Some(agent), Some(digest), Some(bytes)) = (agent, digest, bytes) {
                    digests.insert(agent.to_string(), (digest, bytes));
                }
            }
            Some(_) => {
                if let Some(agent) = agent {
                    digests.remove(agent);
                }
            }
            None => {}
        }
    }
    digests
}

/// Append the leaving lines of one locked act to the queue log,
/// in the order the waits stand in the given list, recovering
/// each wait's request digest from its joining line and carrying
/// the furnished span where the act furnished the wait.
fn append_leavings(
    stack: &Path,
    at: &str,
    kind: &'static str,
    waits: &[Wait],
    furnished_spans: Option<&BTreeMap<String, (i64, i64)>>,
) -> Result<()> {
    if waits.is_empty() {
        return Ok(());
    }
    let digests = queued_digests(stack);
    for wait in waits {
        let mut line = QueueLine::for_wait(at, kind, wait);
        if let Some((digest, bytes)) = digests.get(&wait.agent) {
            line.request_sha256 = Some(digest.clone());
            line.request_bytes = Some(*bytes);
        }
        if let Some(spans) = furnished_spans {
            line.ports = spans.get(&wait.agent).copied();
        }
        line.append(stack)?;
    }
    Ok(())
}

/// Settle the profile of a routed claim from its consultation, in
/// the second phase of the envelope of obligation SY-13, outside
/// the stack lock. Returns the settled profile and the verdict
/// label and the confidence and the fallback class the routing log
/// records, where the fallback is Fallback::None exactly when the
/// verdict decided.
fn settle(
    description: &str,
    default_profile: Profile,
    transport: Option<&dyn JudgeTransport>,
) -> (Profile, Option<String>, Option<f64>, Fallback) {
    let transport = match transport {
        None => return (default_profile, None, None, Fallback::NoTransport),
        Some(transport) => transport,
    };
    let request = routing_request(description);
    let body = match transport.consult(&request) {
        Ok(body) => body,
        Err(_) => return (default_profile, None, None, Fallback::Transport),
    };
    match classify_reply(&body) {
        Consultation::Verdict(profile, confidence) => (
            profile,
            Some(profile.name().to_string()),
            Some(confidence),
            Fallback::None,
        ),
        Consultation::LowConfidence(profile, confidence) => (
            default_profile,
            Some(profile.name().to_string()),
            Some(confidence),
            Fallback::LowConfidence,
        ),
        Consultation::Abstention(confidence) => (
            default_profile,
            Some("other".to_string()),
            Some(confidence),
            Fallback::Abstention,
        ),
        Consultation::Silence => (default_profile, None, None, Fallback::Silence),
        Consultation::Malformed => (default_profile, None, None, Fallback::Malformed),
    }
}

/// The scoring question of obligation SY-20, which the same
/// judge command answers beside the routing question, the two
/// distinguished by the request document each receives.
pub const SCORING_QUESTION: &str = "score";

/// The eleven declared labels of the scoring question, the ten
/// score levels and the no score label the grammar of SY-20
/// names.
pub const SCORE_LABELS: [&str; 11] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "other",
];

const SCORING_INSTRUCTIONS: &str = "The question asks the judge to score one waiting request for preference when port ranges free up, on a scale of ten levels where the tenth is the strongest preference the scale states. Score from the counted facts and nothing else. A request whose draw can be met only by a long run of ranges that rarely stands free scores toward the top. A request whose draw the next small release can furnish scores toward the bottom. The more requests already waiting for the same draw, the higher this request scores. A request carrying a wait bound scores higher as its bound runs shorter against the number already waiting. The description may raise or lower the score only through facts it states as numerals, and a quality the description merely evokes is not a fact and moves the score not at all. Where the counted facts do not determine a preference, the answer is the no score label, and a guess is a wrong answer. The score was taken when the request joined the queue and measures contention at that moment. Answer with exactly one of the listed options.";

const SCORING_FRAMING: &str = "A furnishing tool holds a queue of requests waiting for port ranges.\nThe judge is to score the one waiting request the counted facts below describe, for preference when ranges free up, applying the doctrine of the question to those facts and to nothing else.\nThe score orders the queue and performs no allocation itself.";

const SCORING_CLOSING: &str = "The labels are 1 and 2 and 3 and 4 and 5 and 6 and 7 and 8 and 9 and 10 and other. The levels run from the weakest preference at 1 to the strongest at 10, and other is the no score label, the answer where the counted facts do not determine a preference.";

/// The state text of one scoring request, the vignette of
/// obligation SY-20. The text is a pure function of the settled
/// profile, the waits already standing, the free runs of the
/// pool, the wait bound, and the description, and of nothing
/// else, so no agent name and no stack path and no standing
/// claim and no earlier score ever enters a request.
fn scoring_state(
    settled: &str,
    draw: i64,
    standing: &[Wait],
    runs: &[i64],
    wait_for: Option<i64>,
    description: Option<&str>,
) -> String {
    let mut count_one = 0;
    let mut count_two = 0;
    let mut count_four = 0;
    for wait in standing {
        match settled_draw(&wait.profile) {
            1 => count_one += 1,
            2 => count_two += 1,
            _ => count_four += 1,
        }
    }
    let runs_text = if runs.is_empty() {
        "No run stands free in the pool.".to_string()
    } else {
        let listed: Vec<String> = runs.iter().map(i64::to_string).collect();
        format!(
            "The free runs standing in the pool, in ranges, are {}.",
            listed.join(" and ")
        )
    };
    let bound_text = match wait_for {
        Some(bound) => format!("The request carries a wait bound of {bound} seconds."),
        None => "The request carries no wait bound.".to_string(),
    };
    let description_text = match description {
        Some(text) => text.to_string(),
        None => "The request carries no description and stands on its counted facts alone."
            .to_string(),
    };
    format!(
        "{SCORING_FRAMING}\n\nThe request that waits settled on the {settled} profile, a draw of {draw} ranges.\nWaits already standing, {}. Standing for a draw of 1 range, {count_one}. Standing for a draw of 2 ranges, {count_two}. Standing for a draw of 4 ranges, {count_four}.\n{runs_text}\n{bound_text}\n\n{description_text}\n\n{SCORING_CLOSING}",
        standing.len()
    )
}

/// Build the scoring request document for one waiting claim, in
/// the shape of the routing request document, pinning the model
/// of obligation SY-20 and carrying the vignette as its state.
pub fn scoring_request(
    settled: &str,
    draw: i64,
    standing: &[Wait],
    runs: &[i64],
    wait_for: Option<i64>,
    description: Option<&str>,
) -> String {
    let criteria_text = [
        ("1", "The bottom of the scale, a request the next small release can furnish, standing under no contention worth the name."),
        ("2", "The bottom band, a request a small release can furnish, standing under light contention."),
        ("3", "The top of the bottom band, a request a modest release can furnish, standing under some contention."),
        ("4", "The bottom of the middle band, a request whose draw wants more than the next small release, standing under ordinary contention."),
        ("5", "The middle of the scale, a request whose draw and contention stand in balance."),
        ("6", "The middle band, a request whose draw or whose contention stands above the ordinary."),
        ("7", "The top of the middle band, a request whose draw is hard to furnish or whose contention stands heavy."),
        ("8", "The bottom of the top band, a request whose draw only a long run can furnish, or one standing under heavy contention."),
        ("9", "The top band, a request whose draw rarely stands free, standing under heavy contention."),
        ("10", "The strongest preference the scale states, a request whose draw can be met only by a long run that rarely stands free, standing under the heaviest contention."),
        ("other", "The no score label, the answer where the counted facts leave the preference undetermined."),
    ];
    let mut criteria = serde_json::Map::new();
    for (label, text) in criteria_text {
        criteria.insert(label.to_string(), text.into());
    }
    let mut question = serde_json::Map::new();
    question.insert("criteria".to_string(), criteria.into());
    question.insert("instructions".to_string(), SCORING_INSTRUCTIONS.into());
    question.insert("type".to_string(), "choice".into());
    let mut questions = serde_json::Map::new();
    questions.insert(SCORING_QUESTION.to_string(), question.into());
    let mut document = serde_json::Map::new();
    document.insert("model".to_string(), ROUTING_MODEL.into());
    document.insert("questions".to_string(), questions.into());
    document.insert(
        "state".to_string(),
        scoring_state(settled, draw, standing, runs, wait_for, description).into(),
    );
    serde_json::to_string(&serde_json::Value::Object(document)).unwrap()
}

/// What a scoring reply body classifies as under the grammar of
/// SY-20.
enum ScoreConsultation {
    Score(i64, f64),
    LowConfidence(i64, f64),
    Abstention(f64),
    Silence,
    Malformed,
}

/// Classify one scoring reply body. The grammar is the routing
/// grammar's, one JSON object carrying one answer for the
/// scoring question, whose choice names one of the eleven
/// declared labels and whose probabilities carry exactly those
/// eleven labels each standing in the range 0 to 1. The
/// confidence of a score is the probability of the chosen label
/// read from the probabilities map, and a confidence field the
/// reply also carries is never read.
fn classify_score(body: &str) -> ScoreConsultation {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return ScoreConsultation::Silence;
    }
    let data: JVal = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return ScoreConsultation::Malformed,
    };
    if first_duplicate_key(&data).is_some() {
        return ScoreConsultation::Malformed;
    }
    let entry = match data
        .get("answers")
        .and_then(|answers| answers.get(SCORING_QUESTION))
    {
        Some(entry) => entry,
        None => return ScoreConsultation::Silence,
    };
    if !matches!(entry, JVal::Obj(_)) {
        return ScoreConsultation::Malformed;
    }
    let choice = match entry.get("choice").and_then(JVal::as_str) {
        Some(choice) if SCORE_LABELS.contains(&choice) => choice.to_string(),
        _ => return ScoreConsultation::Malformed,
    };
    let probabilities = match entry.get("probabilities") {
        Some(probabilities @ JVal::Obj(pairs)) if exact_keys(pairs, &SCORE_LABELS) => {
            probabilities
        }
        _ => return ScoreConsultation::Malformed,
    };
    let mut values = [0.0_f64; 11];
    for (slot, label) in values.iter_mut().zip(SCORE_LABELS.iter()) {
        match probabilities.get(label).and_then(jval_f64) {
            Some(number) if (0.0..=1.0).contains(&number) => *slot = number,
            _ => return ScoreConsultation::Malformed,
        }
    }
    let index = SCORE_LABELS
        .iter()
        .position(|label| *label == choice)
        .unwrap();
    let confidence = values[index];
    if choice == "other" {
        ScoreConsultation::Abstention(confidence)
    } else if confidence >= LOW_CONFIDENCE_THRESHOLD {
        ScoreConsultation::Score(choice.parse().unwrap(), confidence)
    } else {
        ScoreConsultation::LowConfidence(choice.parse().unwrap(), confidence)
    }
}

/// The outcome of the one scoring consultation of a wait, the
/// score and its confidence where one stands and the fallback
/// word of obligation SY-20 where none does.
struct ScoreOutcome {
    score: Option<i64>,
    confidence: Option<f64>,
    fallback: &'static str,
    model: Option<String>,
}

/// Score one waiting claim through the transport seam of
/// obligation SY-16, outside the stack lock. With no transport
/// configured the wait records no score under the no transport
/// class and carries a null model name, and every fallback class
/// leaves the wait scoreless.
fn settle_score(transport: Option<&dyn JudgeTransport>, request: &str) -> ScoreOutcome {
    let transport = match transport {
        None => {
            return ScoreOutcome {
                score: None,
                confidence: None,
                fallback: "no transport",
                model: None,
            }
        }
        Some(transport) => transport,
    };
    let model = Some(ROUTING_MODEL.to_string());
    let body = match transport.consult(request) {
        Ok(body) => body,
        Err(_) => {
            return ScoreOutcome {
                score: None,
                confidence: None,
                fallback: "transport",
                model,
            }
        }
    };
    match classify_score(&body) {
        ScoreConsultation::Score(level, confidence) => ScoreOutcome {
            score: Some(level),
            confidence: Some(confidence),
            fallback: "none",
            model,
        },
        ScoreConsultation::LowConfidence(_, _) => ScoreOutcome {
            score: None,
            confidence: None,
            fallback: "low confidence",
            model,
        },
        ScoreConsultation::Abstention(_) => ScoreOutcome {
            score: None,
            confidence: None,
            fallback: "abstention",
            model,
        },
        ScoreConsultation::Silence => ScoreOutcome {
            score: None,
            confidence: None,
            fallback: "silence",
            model,
        },
        ScoreConsultation::Malformed => ScoreOutcome {
            score: None,
            confidence: None,
            fallback: "malformed",
            model,
        },
    }
}

/// Allocate a claim under a named declared profile, with no
/// consultation at all, by the same path a plain claim takes with
/// the draw of the profile.
pub fn claim_profile(stack: &Path, agent: &str, profile: Profile) -> Result<Record> {
    claim_profile_with(stack, agent, profile, &ProcessGit, &save_registry)
}

fn claim_profile_with(
    stack: &Path,
    agent: &str,
    profile: Profile,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<Record> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let (lock, mut data) = claim_prologue(&stack, git)?;
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    refuse_standing_position(&data, agent)?;
    allocate_locked(AllocParts {
        stack: &stack,
        agent,
        draw: profile.draw(),
        data,
        lock: &lock,
        git,
        saver,
        routing: None,
        expired,
        instant,
    })
}

/// Allocate a claim whose profile a model settles from a job
/// description, inside the three phase envelope of obligation
/// SY-13.
///
/// The first phase settles every refusal that does not depend on
/// the verdict under the stack lock and spends no call. The
/// consultation then takes place outside the lock, on the
/// description alone. The third phase takes the lock again and
/// checks the settled profile furnishable against the registry as
/// it then stands, and a verdict naming a profile that cannot be
/// furnished refuses naming the profile, because the declared
/// default is the landing for a consultation that failed to decide
/// and never for a stack that failed to furnish.
pub fn claim_routed(
    stack: &Path,
    agent: &str,
    description: &str,
    default_profile: Profile,
    transport: Option<&dyn JudgeTransport>,
) -> Result<Record> {
    claim_routed_with(
        stack,
        agent,
        description,
        default_profile,
        transport,
        &ProcessGit,
        &save_registry,
    )
}

/// Append the routing line of a routed claim refused under the
/// lock again in the third phase of the envelope of obligation
/// SY-13, after its consultation stood. The line carries the
/// verdict and the confidence the consultation returned and the
/// refusal class SY-15 names for the refusal, and allocates
/// nothing. The refusal itself returns through the caller, so an
/// append failure stands in its place, as it does for the pool
/// refusal of the first phase.
#[allow(clippy::too_many_arguments)]
fn refuse_after_consultation(
    stack: &Path,
    agent: &str,
    description: &str,
    model: &Option<String>,
    verdict: &Option<String>,
    confidence: Option<f64>,
    fallback: Fallback,
    refusal: &str,
) -> Result<()> {
    let mut line = RoutingLine::new(agent, description, model.clone());
    line.verdict = verdict.clone();
    line.confidence = confidence;
    line.fallback = fallback;
    line.refusal = Some(refusal.to_string());
    line.append(stack)
}

fn claim_routed_with(
    stack: &Path,
    agent: &str,
    description: &str,
    default_profile: Profile,
    transport: Option<&dyn JudgeTransport>,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<Record> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let model = transport.map(|_| ROUTING_MODEL.to_string());
    {
        let (lock, mut data) = claim_prologue(&stack, git)?;
        let _ = sweep_expired(&mut data, &now_stamp());
        refuse_standing_position(&data, agent)?;
        validate_description(description)?;
        if !PROFILES
            .iter()
            .any(|profile| free_run(&data, profile.draw()).is_some())
        {
            let mut line = RoutingLine::new(agent, description, model.clone());
            line.fallback = Fallback::NoConsultation;
            line.refusal = Some("pool".to_string());
            line.append(&stack)?;
            return Err(Error::plain(
                "the port pool is exhausted, no declared profile can be furnished",
            ));
        }
        drop(lock);
    }
    let (settled, verdict, confidence, fallback) = settle(description, default_profile, transport);
    let lock = locked(&stack)?;
    let mut data = match load_registry(&stack) {
        Ok(data) => data,
        Err(exc) => {
            refuse_after_consultation(
                &stack,
                agent,
                description,
                &model,
                &verdict,
                confidence,
                fallback,
                "corrupt",
            )?;
            return Err(exc);
        }
    };
    if let Err(exc) = verify_own_repo(git, &stack) {
        refuse_after_consultation(
            &stack,
            agent,
            description,
            &model,
            &verdict,
            confidence,
            fallback,
            "repo",
        )?;
        return Err(exc);
    }
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    if data.claims.contains_key(agent) || data.waits.iter().any(|wait| wait.agent == agent) {
        refuse_after_consultation(
            &stack,
            agent,
            description,
            &model,
            &verdict,
            confidence,
            fallback,
            "duplicate",
        )?;
        return Err(standing_claim_error(agent));
    }
    if free_run(&data, settled.draw()).is_none() {
        let mut line = RoutingLine::new(agent, description, model.clone());
        line.verdict = verdict.clone();
        line.confidence = confidence;
        line.fallback = fallback;
        line.refusal = Some("pool".to_string());
        line.append(&stack)?;
        return Err(Error::plain(if fallback == Fallback::None {
            format!(
                "profile {} cannot be furnished, its draw does not stand free in the port pool",
                settled.name()
            )
        } else {
            format!(
                "the default profile {} cannot be furnished, its draw does not stand free in the port pool",
                settled.name()
            )
        }));
    }
    let mut line = RoutingLine::new(agent, description, model);
    line.verdict = verdict;
    line.confidence = confidence;
    line.fallback = fallback;
    line.allocated = Some(settled.name().to_string());
    allocate_locked(AllocParts {
        stack: &stack,
        agent,
        draw: settled.draw(),
        data,
        lock: &lock,
        git,
        saver,
        routing: Some(&line),
        expired,
        instant,
    })
}

/// What a waiting claim did, of obligation SY-17. A waiting
/// claim whose settled draw stands free allocates exactly as
/// without the option and returns the claim record, and a
/// waiting claim whose draw does not stand free records a wait
/// and returns the wait record, creating no worktree and no
/// branch and no database and no env file and taking no port
/// span.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    Claim(Record),
    Wait(Wait),
}

/// Allocate a plain claim that waits where its draw does not
/// stand free, under the waiting envelope of obligation SY-19.
pub fn claim_waiting(
    stack: &Path,
    agent: &str,
    wait_for: Option<i64>,
    transport: Option<&dyn JudgeTransport>,
) -> Result<ClaimOutcome> {
    claim_waiting_with(stack, agent, wait_for, transport, &ProcessGit, &save_registry)
}

fn claim_waiting_with(
    stack: &Path,
    agent: &str,
    wait_for: Option<i64>,
    transport: Option<&dyn JudgeTransport>,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<ClaimOutcome> {
    claim_waiting_core(
        stack,
        agent,
        "plain",
        1,
        None,
        wait_for,
        transport,
        git,
        saver,
    )
}

/// Allocate a claim under a named declared profile that waits
/// where its draw does not stand free.
pub fn claim_profile_waiting(
    stack: &Path,
    agent: &str,
    profile: Profile,
    wait_for: Option<i64>,
    transport: Option<&dyn JudgeTransport>,
) -> Result<ClaimOutcome> {
    claim_profile_waiting_with(
        stack,
        agent,
        profile,
        wait_for,
        transport,
        &ProcessGit,
        &save_registry,
    )
}

#[allow(clippy::too_many_arguments)]
fn claim_profile_waiting_with(
    stack: &Path,
    agent: &str,
    profile: Profile,
    wait_for: Option<i64>,
    transport: Option<&dyn JudgeTransport>,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<ClaimOutcome> {
    claim_waiting_core(
        stack,
        agent,
        profile.name(),
        profile.draw(),
        None,
        wait_for,
        transport,
        git,
        saver,
    )
}

/// The waiting envelope of obligation SY-19 for a claim whose
/// settled profile and draw already stand determined, the plain
/// and profiled shapes. The first phase runs under the lock and
/// spends no call, the scoring consultation runs outside the
/// lock, and the third phase takes the lock again and allocates
/// or records the wait against the registry as it then stands.
#[allow(clippy::too_many_arguments)]
fn claim_waiting_core(
    stack: &Path,
    agent: &str,
    settled: &str,
    draw: i64,
    description: Option<&str>,
    wait_for: Option<i64>,
    transport: Option<&dyn JudgeTransport>,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<ClaimOutcome> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    validate_wait_bound(wait_for)?;
    let snapshot = {
        let (lock, mut data) = claim_prologue(&stack, git)?;
        let instant = now_stamp();
        let expired = sweep_expired(&mut data, &instant);
        refuse_standing_position(&data, agent)?;
        refuse_queue_full(&data)?;
        if free_run(&data, draw).is_some() {
            let record = allocate_locked(AllocParts {
                stack: &stack,
                agent,
                draw,
                data,
                lock: &lock,
                git,
                saver,
                routing: None,
                expired,
                instant,
            })?;
            return Ok(ClaimOutcome::Claim(record));
        }
        (data.waits.clone(), free_runs(&data))
    };
    let request = scoring_request(settled, draw, &snapshot.0, &snapshot.1, wait_for, description);
    let outcome = settle_score(transport, &request);
    let lock = locked(&stack)?;
    let mut data = load_registry(&stack)?;
    verify_own_repo(git, &stack)?;
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    refuse_standing_position(&data, agent)?;
    refuse_queue_full(&data)?;
    if free_run(&data, draw).is_some() {
        let record = allocate_locked(AllocParts {
            stack: &stack,
            agent,
            draw,
            data,
            lock: &lock,
            git,
            saver,
            routing: None,
            expired,
            instant,
        })?;
        return Ok(ClaimOutcome::Claim(record));
    }
    let wait = Wait {
        agent: agent.to_string(),
        profile: settled.to_string(),
        recorded_at: instant.clone(),
        wait_for,
        score: outcome.score,
        confidence: outcome.confidence,
        model: outcome.model,
        fallback: outcome.fallback.to_string(),
    };
    let mut line = QueueLine::for_wait(&instant, "queued", &wait);
    line.request_sha256 = Some(sha256_hex(request.as_bytes()));
    line.request_bytes = Some(request.len());
    line.append(&stack)?;
    append_leavings(&stack, &instant, "expired", &expired, None)?;
    data.waits.push(wait.clone());
    lock.verify_current()?;
    saver(&stack, &data)?;
    Ok(ClaimOutcome::Wait(wait))
}

/// Allocate a routed claim that waits where its settled draw
/// does not stand free, under the waiting envelope of obligation
/// SY-19. A routed claim refused in the first phase because no
/// declared profile could be furnished settles for waiting
/// purposes on its declared default and proceeds to wait without
/// a routing consultation, and its routing line carries the no
/// consultation fallback word.
#[allow(clippy::too_many_arguments)]
pub fn claim_routed_waiting(
    stack: &Path,
    agent: &str,
    description: &str,
    default_profile: Profile,
    transport: Option<&dyn JudgeTransport>,
    wait_for: Option<i64>,
) -> Result<ClaimOutcome> {
    claim_routed_waiting_with(
        stack,
        agent,
        description,
        default_profile,
        transport,
        wait_for,
        &ProcessGit,
        &save_registry,
    )
}

#[allow(clippy::too_many_arguments)]
fn claim_routed_waiting_with(
    stack: &Path,
    agent: &str,
    description: &str,
    default_profile: Profile,
    transport: Option<&dyn JudgeTransport>,
    wait_for: Option<i64>,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<ClaimOutcome> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    validate_wait_bound(wait_for)?;
    let model = transport.map(|_| ROUTING_MODEL.to_string());
    let mut pre_settled: Option<Profile> = None;
    let snapshot = {
        let (lock, mut data) = claim_prologue(&stack, git)?;
        let instant = now_stamp();
        let _ = sweep_expired(&mut data, &instant);
        refuse_standing_position(&data, agent)?;
        validate_description(description)?;
        refuse_queue_full(&data)?;
        if !PROFILES
            .iter()
            .any(|profile| free_run(&data, profile.draw()).is_some())
        {
            pre_settled = Some(default_profile);
        }
        (data.waits.clone(), free_runs(&data))
    };
    let (settled, verdict, confidence, fallback) = match pre_settled {
        Some(profile) => (profile, None, None, Fallback::NoConsultation),
        None => settle(description, default_profile, transport),
    };
    let request = scoring_request(
        settled.name(),
        settled.draw(),
        &snapshot.0,
        &snapshot.1,
        wait_for,
        Some(description),
    );
    let outcome = settle_score(transport, &request);
    let lock = locked(&stack)?;
    let mut data = match load_registry(&stack) {
        Ok(data) => data,
        Err(exc) => {
            refuse_after_consultation(
                &stack,
                agent,
                description,
                &model,
                &verdict,
                confidence,
                fallback,
                "corrupt",
            )?;
            return Err(exc);
        }
    };
    if let Err(exc) = verify_own_repo(git, &stack) {
        refuse_after_consultation(
            &stack,
            agent,
            description,
            &model,
            &verdict,
            confidence,
            fallback,
            "repo",
        )?;
        return Err(exc);
    }
    let instant = now_stamp();
    let expired = sweep_expired(&mut data, &instant);
    if data.claims.contains_key(agent) || data.waits.iter().any(|wait| wait.agent == agent) {
        refuse_after_consultation(
            &stack,
            agent,
            description,
            &model,
            &verdict,
            confidence,
            fallback,
            "duplicate",
        )?;
        return Err(standing_claim_error(agent));
    }
    if data.waits.len() >= QUEUE_BOUND {
        let mut line = RoutingLine::new(agent, description, model.clone());
        line.verdict = verdict.clone();
        line.confidence = confidence;
        line.fallback = fallback;
        line.refusal = Some("queue full".to_string());
        line.append(&stack)?;
        return Err(queue_full_error());
    }
    if free_run(&data, settled.draw()).is_some() {
        let mut line = RoutingLine::new(agent, description, model);
        line.verdict = verdict;
        line.confidence = confidence;
        line.fallback = fallback;
        line.allocated = Some(settled.name().to_string());
        let record = allocate_locked(AllocParts {
            stack: &stack,
            agent,
            draw: settled.draw(),
            data,
            lock: &lock,
            git,
            saver,
            routing: Some(&line),
            expired,
            instant,
        })?;
        return Ok(ClaimOutcome::Claim(record));
    }
    let wait = Wait {
        agent: agent.to_string(),
        profile: settled.name().to_string(),
        recorded_at: instant.clone(),
        wait_for,
        score: outcome.score,
        confidence: outcome.confidence,
        model: outcome.model,
        fallback: outcome.fallback.to_string(),
    };
    let mut line = QueueLine::for_wait(&instant, "queued", &wait);
    line.request_sha256 = Some(sha256_hex(request.as_bytes()));
    line.request_bytes = Some(request.len());
    line.append(&stack)?;
    append_leavings(&stack, &instant, "expired", &expired, None)?;
    let mut routing_line = RoutingLine::new(agent, description, model);
    routing_line.verdict = verdict;
    routing_line.confidence = confidence;
    routing_line.fallback = fallback;
    routing_line.refusal = Some("queued".to_string());
    routing_line.append(&stack)?;
    data.waits.push(wait.clone());
    lock.verify_current()?;
    saver(&stack, &data)?;
    Ok(ClaimOutcome::Wait(wait))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fabricated_env() -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = GIT_LOCATION_VARIABLES
            .iter()
            .chain(GIT_CONFIG_VARIABLES.iter())
            .map(|name| (name.to_string(), "/elsewhere".to_string()))
            .collect();
        for name in [
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_12",
            "GIT_CONFIG_VALUE_12",
        ] {
            vars.push((name.to_string(), "steered".to_string()));
        }
        vars.push((
            "SWITCHYARD_UNRELATED_MARKER".to_string(),
            "kept".to_string(),
        ));
        vars
    }

    #[test]
    fn the_git_helper_scrubs_location_variables() {
        let env = git_env_from(fabricated_env());
        for name in GIT_LOCATION_VARIABLES {
            assert!(!env.iter().any(|(key, _)| key == name), "{name}");
        }
        assert!(env
            .iter()
            .any(|(key, value)| key == "SWITCHYARD_UNRELATED_MARKER" && value == "kept"));
    }

    #[test]
    fn the_git_helper_scrubs_config_variables_and_numbered_pairs() {
        let env = git_env_from(fabricated_env());
        for name in GIT_CONFIG_VARIABLES {
            assert!(!env.iter().any(|(key, _)| key == name), "{name}");
        }
        for name in [
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_12",
            "GIT_CONFIG_VALUE_12",
        ] {
            assert!(!env.iter().any(|(key, _)| key == name), "{name}");
        }
        assert!(env
            .iter()
            .any(|(key, value)| key == "SWITCHYARD_UNRELATED_MARKER" && value == "kept"));
    }

    #[test]
    fn a_git_call_that_times_out_is_a_clean_error() {
        let dir = std::env::temp_dir().join(format!(
            "switchyard-unit-test-{}-slow-git",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake_bin = dir.join("bin");
        fs::create_dir_all(&fake_bin).unwrap();
        // The fake git is staged under a temporary name and
        // renamed into place, so the spawn can never race the
        // writer still holding the file open.
        let staged_git = fake_bin.join("git.staged");
        fs::write(&staged_git, "#!/bin/sh\nsleep 10\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_git, fs::Permissions::from_mode(0o755)).unwrap();
        let fake_git = fake_bin.join("git");
        fs::rename(&staged_git, &fake_git).unwrap();
        let real_git = dir.join("repo");
        fs::create_dir_all(&real_git).unwrap();
        let env = vec![(
            "PATH".to_string(),
            format!("{}:/usr/bin:/bin", fake_bin.display()),
        )];
        let result = run_git(&real_git, &["status"], &env, Duration::from_secs(1));
        let _ = fs::remove_dir_all(&dir);
        let error = result.expect_err("the sleeping git must time out");
        assert_eq!(error.exit_code(), 1);
        assert!(
            error.message().contains("did not finish within 1 seconds"),
            "{}",
            error.message()
        );
    }

    struct BusyBranchGit;

    impl GitRunner for BusyBranchGit {
        fn result(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
            if args.len() >= 2 && args[0] == "branch" && args[1] == "-D" {
                return Ok(GitOutput {
                    code: 1,
                    stdout: String::new(),
                    stderr: "the branch is busy".to_string(),
                });
            }
            ProcessGit.result(repo, args)
        }
    }

    #[test]
    fn a_failed_claim_reports_an_incomplete_rollback() {
        let dir = std::env::temp_dir().join(format!(
            "switchyard-unit-test-{}-rollback",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let stack = dir.join("stack");
        init_stack(
            &stack,
            DEFAULT_PORT_START as i128,
            DEFAULT_RANGE_SIZE as i128,
            DEFAULT_RANGE_COUNT as i128,
        )
        .unwrap();
        let broken_save = |_: &Path, _: &Registry| -> Result<()> {
            Err(Error::plain("the registry write is broken in this test"))
        };
        let error = claim_with(&stack, "alpha", &BusyBranchGit, &broken_save)
            .expect_err("the claim must fail when the save fails");
        let _ = fs::remove_dir_all(&dir);
        let message = error.message().to_string();
        assert!(
            message.contains("rolling it back was incomplete"),
            "{message}"
        );
        assert!(message.contains("could not be deleted"), "{message}");
    }

    #[test]
    fn timestamps_the_format_writes_parse_and_garbage_does_not() {
        assert!(is_valid_timestamp(&now_stamp()));
        assert!(is_valid_timestamp("2026-11-02T00:00:00+00:00"));
        assert!(is_valid_timestamp("2026-11-02"));
        assert!(!is_valid_timestamp("not a timestamp"));
        assert!(!is_valid_timestamp("2026-13-02T00:00:00+00:00"));
        assert!(!is_valid_timestamp("2026-02-30T00:00:00+00:00"));
        assert!(!is_valid_timestamp(""));
    }

    #[test]
    fn a_replaced_lock_file_fails_the_identity_check() {
        let dir = std::env::temp_dir().join(format!(
            "switchyard-unit-test-{}-lock-identity",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let stack = dir.join("stack");
        init_stack(
            &stack,
            DEFAULT_PORT_START as i128,
            DEFAULT_RANGE_SIZE as i128,
            DEFAULT_RANGE_COUNT as i128,
        )
        .unwrap();
        let held = locked(&stack).unwrap();
        held.verify_current()
            .expect("the lock just taken names the file it holds");
        fs::remove_file(stack.join(LOCK_NAME)).unwrap();
        fs::write(stack.join(LOCK_NAME), "").unwrap();
        let error = held
            .verify_current()
            .expect_err("a replaced lock file is a different file");
        drop(held);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(error.exit_code(), 1);
        assert!(
            error
                .message()
                .contains("is not the file this command locked"),
            "{}",
            error.message()
        );
    }

    /// A git seam staging the lock tamper from the repair brief.
    /// On the first worktree add it deletes the lock file under
    /// the in flight claim and runs a second claim to completion,
    /// the way a racing claim against a fresh lock file and the
    /// not yet saved registry would.
    struct LockReplacingGit {
        stack: PathBuf,
        fired: std::cell::Cell<bool>,
    }

    impl GitRunner for LockReplacingGit {
        fn result(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
            if !self.fired.get() && args.first() == Some(&"worktree") && args.get(1) == Some(&"add")
            {
                self.fired.set(true);
                fs::remove_file(self.stack.join(LOCK_NAME)).unwrap();
                claim(&self.stack, "beta")
                    .expect("the nested claim completes against the fresh lock");
            }
            ProcessGit.result(repo, args)
        }
    }

    #[test]
    fn a_claim_whose_lock_was_replaced_fails_loudly_and_rolls_back() {
        let dir = std::env::temp_dir().join(format!(
            "switchyard-unit-test-{}-lock-replaced",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let stack = dir.join("stack");
        init_stack(&stack, 26000, 5, 4).unwrap();
        let git = LockReplacingGit {
            stack: stack.clone(),
            fired: std::cell::Cell::new(false),
        };
        let error = claim_with(&stack, "alpha", &git, &save_registry)
            .expect_err("the claim under a replaced lock must fail");
        assert_eq!(error.exit_code(), 1);
        assert!(
            error
                .message()
                .contains("is not the file this command locked"),
            "{}",
            error.message()
        );
        let data = load_registry(&stack).unwrap();
        assert_eq!(data.claims.len(), 1);
        let beta = data
            .claims
            .get("beta")
            .expect("only the second claim is recorded");
        assert_eq!((beta.port_start, beta.port_end), (26000, 26004));
        assert!(stack.join("worktrees/beta").is_dir());
        assert!(!stack.join("worktrees/alpha").exists());
        let listing = ProcessGit
            .result(&stack.join(REPO_NAME), &["branch", "--list"])
            .unwrap();
        assert!(
            listing.stdout.contains("switchyard/beta"),
            "{}",
            listing.stdout
        );
        assert!(
            !listing.stdout.contains("switchyard/alpha"),
            "{}",
            listing.stdout
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
