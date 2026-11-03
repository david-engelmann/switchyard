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

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub const REGISTRY_NAME: &str = "switchyard-registry.json";
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
    if !exact_keys(pairs, &REGISTRY_KEYS) {
        return Err(corrupt(
            path,
            "the top level fields are not exactly version and port_pool and claims",
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
        if (port_start - pool.start) % pool.range_size != 0
            || port_end != port_start + pool.range_size - 1
        {
            return Err(corrupt(
                path,
                format!(
                    "the claim for {agent} does not hold exactly one slot of the port pool grid"
                ),
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
    Ok(Registry {
        version: REGISTRY_VERSION,
        port_pool: pool,
        claims,
    })
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

fn free_range(data: &Registry) -> Option<(i64, i64)> {
    let pool = &data.port_pool;
    let taken: Vec<(i64, i64)> = data
        .claims
        .values()
        .map(|record| (record.port_start, record.port_end))
        .collect();
    for index in 0..pool.range_count {
        let start = pool.start + index * pool.range_size;
        let end = start + pool.range_size - 1;
        if taken.iter().all(|(low, high)| end < *low || start > *high) {
            return Some((start, end));
        }
    }
    None
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

fn claim_with(
    stack: &Path,
    agent: &str,
    git: &dyn GitRunner,
    saver: &dyn Fn(&Path, &Registry) -> Result<()>,
) -> Result<Record> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let repo = stack.join(REPO_NAME);
    let branch = branch_name(agent);
    let worktree_rel = format!("{WORKTREES_NAME}/{agent}");
    let worktree = stack.join(&worktree_rel);
    let env_rel = format!("{worktree_rel}/{ENV_NAME}");
    let lock = locked(&stack)?;
    let mut data = load_registry(&stack)?;
    verify_own_repo(git, &stack)?;
    if data.claims.contains_key(agent) {
        return Err(Error::plain(format!(
            "agent {agent} already holds a claim in this stack"
        )));
    }
    let (port_start, port_end) = match free_range(&data) {
        Some(range) => range,
        None => {
            return Err(Error::plain(
                "the port pool is exhausted, every range is held",
            ))
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
        claimed_at: now_stamp(),
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
        write_env_file(&stack, &env_rel, &record)?;
        data.claims.insert(agent.to_string(), record.clone());
        lock.verify_current()?;
        saver(&stack, &data)?;
        Ok(())
    })();
    match attempt {
        Ok(()) => Ok(record),
        Err(exc) => {
            let problems = rollback_claim(git, &repo, &worktree, &branch, branch_preexisted);
            if !problems.is_empty() {
                return Err(Error::plain(format!(
                    "the claim for {agent} failed, {}, and rolling it back was incomplete, {}",
                    exc.message(),
                    problems.join(", ")
                )));
            }
            Err(exc)
        }
    }
}

/// Return every resource a claim holds and remove its record.
pub fn release(stack: &Path, agent: &str) -> Result<Record> {
    release_with(stack, agent, &ProcessGit)
}

fn release_with(stack: &Path, agent: &str, git: &dyn GitRunner) -> Result<Record> {
    let stack = resolve_path(stack);
    validate_agent_name(agent)?;
    let repo = stack.join(REPO_NAME);
    let lock = locked(&stack)?;
    let mut data = load_registry(&stack)?;
    verify_own_repo(git, &stack)?;
    let record = match data.claims.get(agent) {
        Some(record) => record.clone(),
        None => {
            return Err(Error::plain(format!(
                "agent {agent} holds no claim in this stack"
            )))
        }
    };
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
    lock.verify_current()?;
    save_registry(&stack, &data)?;
    Ok(record)
}

/// Return the recorded claims sorted by agent, reading nothing
/// else.
pub fn list_claims(stack: &Path) -> Result<Vec<Record>> {
    let stack = resolve_path(stack);
    let data = load_registry(&stack)?;
    Ok(data.claims.values().cloned().collect())
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
