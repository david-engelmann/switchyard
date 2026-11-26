//! Command line interface for switchyard.
//!
//! One stack per directory, four commands. Init creates the stack,
//! claim allocates, list shows who holds what, and release returns
//! everything. A claim is plain, or profiled under a declared
//! profile, or routed by a job description a judge settles, and any
//! claim may wait where its draw does not stand free. List may
//! print the queue of standing waits instead of the claims.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{
    active_child_group, claim, claim_profile, claim_profile_waiting, claim_routed,
    claim_routed_waiting, claim_waiting, init_stack, list_claims, list_waits, profile_named,
    release_report, resolve_path, ClaimOutcome, JudgeTransport, Record, ReleaseOutcome, Result,
    Wait, DEFAULT_PORT_START, DEFAULT_RANGE_COUNT, DEFAULT_RANGE_SIZE, JUDGE_TIMEOUT_SECONDS,
};

const COMMANDS: [&str; 4] = ["init", "claim", "list", "release"];

const HELP: &str = "usage: switchyard [-h] {init,claim,list,release} ...

Give every parallel agent its own branch, port range, database, and
env file with one command.

commands:
  init     create a stack root with a repository and a registry
  claim    allocate a worktree, a range, a database, and an env file
  list     print who holds what, derived only from the registry
  release  return every resource an agent holds

stack option:
  --stack PATH             the stack root the command works on,
                           required beside every command

init options:
  --port-start PORT        the first port of the pool, 20000 by
                           default
  --range-size COUNT       the ports in one range, 10 by default
  --range-count COUNT      the ranges in the pool, 16 by default

claim options:
  --profile NAME           allocate under a declared profile, one of
                           lean, standard, or roomy, with no consultation
  --describe TEXT          route the claim by a job description the
                           pinned decision model judges
  --default-profile NAME   the profile a routed claim lands on when the
                           consultation does not decide, required
                           beside --describe
  --judge-command PROGRAM  the one program a claim consults, named
                           beside --describe for the routing question
                           or beside --wait for the scoring question
  --judge-arg VALUE        one argument passed to --judge-command,
                           repeatable in order beside --judge-command
  --wait                   record a wait in the queue where the draw
                           does not stand free instead of refusing,
                           creating nothing and taking no port span
  --wait-for SECONDS       bound the wait by a positive number of
                           seconds, named beside --wait

list options:
  --queue                  print the standing waits in offer order
                           instead of the claims table

A judge command is spawned with any --judge-arg arguments in the order
they were given. It receives the request document as one line on its
standard input and answers with the reply document on its standard
output, inside a deadline of 30 seconds. A
judge command that cannot be started, or whose request cannot be
written, or that answers nothing usable, or that runs past the
deadline leaves the claim on its declared default, and the routing
log records the landing. The same program answers the scoring
question of a waiting claim, from a request document that names that
question, and a waiting claim the judge cannot score records its
wait with no score.
";

struct Args {
    command: String,
    agent: Option<String>,
    stack: PathBuf,
    port_start: i128,
    range_size: i128,
    range_count: i128,
    describe: Option<String>,
    default_profile: Option<String>,
    profile: Option<String>,
    judge_command: Option<OsString>,
    judge_args: Vec<OsString>,
    wait: bool,
    wait_for: Option<i128>,
    queue: bool,
}

enum Parsed {
    Run(Box<Args>),
    Exit(i32),
}

/// Parse one integer option's value, refusing a value that is
/// no integer by the option's own name. The three pool options
/// share this refusal, so it stands written once here. A digit
/// string past every machine width saturates inside parse_int
/// and is refused by the library's validation instead, so this
/// refusal fires only for text that is not an integer at all.
fn int_option(option: &str, value: &str) -> std::result::Result<i128, String> {
    match parse_int(value) {
        Some(number) => Ok(number),
        None => Err(format!("argument {option}: invalid int value: '{value}'")),
    }
}

/// Parse an integer at whatever magnitude the caller wrote it.
///
/// Python's parser accepted integers of any size and let the pool
/// validation refuse the absurd ones, so a value that fits no
/// machine integer saturates here instead of failing the parse,
/// and the refusal still comes from validation in the same exit
/// class Python raised it in.
fn parse_int(text: &str) -> Option<i128> {
    match text.parse::<i128>() {
        Ok(value) => Some(value),
        Err(_) => {
            let (sign, digits) = match text.strip_prefix('-') {
                Some(rest) => (-1i128, rest),
                None => (1i128, text.strip_prefix('+').unwrap_or(text)),
            };
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(sign * i128::MAX)
            } else {
                None
            }
        }
    }
}

/// The refusal a failed write of the command's own output
/// surfaces as. The command's prints are checked, so a claim
/// or a list printed into a closed pipe or a full device fails
/// loudly at exit 1 with this refusal, reported on the error
/// stream where that stream still takes writes, instead of
/// truncating silently at exit 0. Rust ignores SIGPIPE for the
/// process, so without the check no surface reports the
/// truncation at all.
fn output_error(exc: std::io::Error) -> crate::Error {
    crate::Error::Plain(format!("the command's output could not be written, {exc}"))
}

/// Print the help text, checked the way every print of the
/// command is checked. A failed help write reports itself on
/// the error stream and exits 1, as a failed print inside a
/// command does.
fn print_help(out: &mut dyn Write, err: &mut dyn Write) -> Parsed {
    match write!(out, "{HELP}") {
        Ok(()) => Parsed::Exit(0),
        Err(exc) => {
            let _ = writeln!(err, "error: {}", output_error(exc).message());
            Parsed::Exit(1)
        }
    }
}

fn is_switchyard_option(bytes: &[u8]) -> bool {
    let head = match bytes.iter().position(|&b| b == b'=') {
        Some(eq) if bytes.starts_with(b"--") => &bytes[..eq],
        _ => bytes,
    };
    matches!(
        head,
        b"-h"
            | b"--help"
            | b"--stack"
            | b"--port-start"
            | b"--range-size"
            | b"--range-count"
            | b"--describe"
            | b"--default-profile"
            | b"--profile"
            | b"--judge-command"
            | b"--judge-arg"
            | b"--wait"
            | b"--wait-for"
            | b"--queue"
    )
}

fn parse(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::result::Result<Parsed, String> {
    let first = match args.first() {
        Some(token) => token,
        None => return Err("the following arguments are required: command".to_string()),
    };
    if first == "-h" || first == "--help" {
        return Ok(print_help(out, err));
    }
    let first_lossy = first.to_string_lossy();
    let command = match first.to_str() {
        Some(name) if COMMANDS.contains(&name) => name.to_string(),
        _ => {
            if first.as_os_str().as_bytes().starts_with(b"-") {
                return Err(format!("unrecognized arguments: {first_lossy}"));
            }
            return Err(format!(
                "invalid choice: '{first_lossy}' (choose from 'init', 'claim', 'list', 'release')"
            ));
        }
    };
    let mut agent: Option<String> = None;
    let mut stack: Option<PathBuf> = None;
    let mut port_start: i128 = DEFAULT_PORT_START as i128;
    let mut range_size: i128 = DEFAULT_RANGE_SIZE as i128;
    let mut range_count: i128 = DEFAULT_RANGE_COUNT as i128;
    let mut describe: Option<String> = None;
    let mut default_profile: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut judge_command: Option<OsString> = None;
    let mut judge_args: Vec<OsString> = Vec::new();
    let mut wait = false;
    let mut wait_for: Option<i128> = None;
    let mut queue = false;
    let mut index = 1;
    while index < args.len() {
        let raw = &args[index];
        let raw_bytes = raw.as_os_str().as_bytes();
        let token = raw.to_string_lossy();
        if raw == "-h" || raw == "--help" {
            return Ok(print_help(out, err));
        }
        if raw == "--wait" && command == "claim" {
            wait = true;
            index += 1;
            continue;
        }
        if raw == "--queue" && command == "list" {
            queue = true;
            index += 1;
            continue;
        }
        let (option, inline_value) = match raw_bytes
            .iter()
            .position(|&b| b == b'=')
            .filter(|_| raw_bytes.starts_with(b"--"))
        {
            Some(eq) => match std::str::from_utf8(&raw_bytes[..eq]) {
                Ok(name) => (
                    name.to_string(),
                    Some(OsString::from_vec(raw_bytes[eq + 1..].to_vec())),
                ),
                Err(_) => (token.clone().into_owned(), None),
            },
            None => (token.clone().into_owned(), None),
        };
        let known_option = matches!(
            option.as_str(),
            "--stack" | "--port-start" | "--range-size" | "--range-count"
        ) && (option == "--stack" || command == "init")
            || matches!(
                option.as_str(),
                "--describe"
                    | "--default-profile"
                    | "--profile"
                    | "--judge-command"
                    | "--judge-arg"
                    | "--wait-for"
            ) && command == "claim";
        if known_option {
            let value = match inline_value {
                Some(value) => value,
                None => {
                    let next = args.get(index + 1);
                    let accepted = match next {
                        Some(candidate) if option == "--judge-arg" => {
                            !is_switchyard_option(candidate.as_os_str().as_bytes())
                        }
                        Some(candidate) => !candidate.as_os_str().as_bytes().starts_with(b"--"),
                        None => false,
                    };
                    if accepted {
                        index += 1;
                        next.unwrap().clone()
                    } else {
                        return Err(format!("argument {option}: expected one argument"));
                    }
                }
            };
            match option.as_str() {
                "--stack" => stack = Some(PathBuf::from(value)),
                "--port-start" => port_start = int_option(&option, &value.to_string_lossy())?,
                "--range-size" => range_size = int_option(&option, &value.to_string_lossy())?,
                "--range-count" => range_count = int_option(&option, &value.to_string_lossy())?,
                "--describe" => {
                    describe = Some(match value.into_string() {
                        Ok(text) => text,
                        Err(_) => {
                            return Err(
                                "argument --describe: the job description is not valid UTF-8"
                                    .to_string(),
                            )
                        }
                    });
                }
                "--default-profile" => default_profile = Some(value.to_string_lossy().into_owned()),
                "--profile" => profile = Some(value.to_string_lossy().into_owned()),
                "--judge-command" => judge_command = Some(value),
                "--judge-arg" => judge_args.push(value),
                "--wait-for" => {
                    let value_text = value.to_string_lossy();
                    wait_for = Some(match parse_int(&value_text) {
                        Some(number) if number > 0 && i64::try_from(number).is_ok() => number,
                        Some(number) if number > 0 => {
                            return Err(format!(
                                "argument --wait-for: a wait bound cannot stand past {} seconds, not '{value_text}'",
                                i64::MAX
                            ))
                        }
                        Some(_) => {
                            return Err(format!(
                                "argument --wait-for: a wait bound is a positive number of seconds, not '{value_text}'"
                            ))
                        }
                        None => {
                            return Err(format!(
                                "argument --wait-for: invalid int value: '{value_text}'"
                            ))
                        }
                    })
                }
                _ => unreachable!(),
            }
            index += 1;
            continue;
        }
        if raw_bytes.starts_with(b"-") && raw_bytes.len() > 1 {
            return Err(format!("unrecognized arguments: {token}"));
        }
        if (command == "claim" || command == "release") && agent.is_none() {
            agent = Some(token.into_owned());
            index += 1;
            continue;
        }
        return Err(format!("unrecognized arguments: {token}"));
    }
    let stack = match stack {
        Some(stack) => stack,
        None => return Err("the following arguments are required: --stack".to_string()),
    };
    if (command == "claim" || command == "release") && agent.is_none() {
        return Err("the following arguments are required: agent".to_string());
    }
    if command == "claim" {
        if profile.is_some() && describe.is_some() {
            return Err(
                "argument --profile: a claim takes either --profile or --describe, not both"
                    .to_string(),
            );
        }
        if describe.is_none() && default_profile.is_some() {
            return Err(
                "argument --default-profile: a routed claim takes --describe beside it".to_string(),
            );
        }
        if !judge_args.is_empty() && judge_command.is_none() {
            return Err(
                "argument --judge-arg: a judge argument takes --judge-command beside it"
                    .to_string(),
            );
        }
        if describe.is_none() && !wait && judge_command.is_some() {
            return Err(
                "argument --judge-command: a judge command takes --describe or --wait beside it"
                    .to_string(),
            );
        }
        if wait_for.is_some() && !wait {
            return Err("argument --wait-for: a wait bound takes --wait beside it".to_string());
        }
        if let Some(description) = &describe {
            if default_profile.is_none() {
                return Err(
                    "argument --describe: a routed claim requires --default-profile".to_string(),
                );
            }
            if description.trim().is_empty() {
                return Err("argument --describe: the job description is empty".to_string());
            }
        }
    }
    Ok(Parsed::Run(Box::new(Args {
        command,
        agent,
        stack,
        port_start,
        range_size,
        range_count,
        describe,
        default_profile,
        profile,
        judge_command,
        judge_args,
        wait,
        wait_for,
        queue,
    })))
}

/// The transport that consults a judge through one named program,
/// the operator's side of the seam of obligation SY-16.
///
/// The program is spawned with any --judge-arg arguments in order,
/// receives the request document as one line on its standard input,
/// and answers with
/// the reply body on its standard output. A spawn failure, a failed
/// request write, a nonzero exit, a run past the deadline, and a
/// reply that is not text each surface as a transport failure of
/// theirs, so the claim lands on its declared default under the
/// transport class. The request write is judged before the reply
/// is read at all, because the recorded request digest must be the
/// request the judge actually judged. The deadline bounds the
/// whole consultation, the collection of the writer and the reader
/// included, because a descendant of the judge holding the reply
/// pipe open cannot stretch a read past it.
struct SubprocessTransport {
    program: OsString,
    args: Vec<OsString>,
}

impl JudgeTransport for SubprocessTransport {
    fn consult(&self, request: &str) -> std::result::Result<String, String> {
        run_judge_command(&self.program, &self.args, request)
    }
}

fn run_judge_command(
    program: &OsStr,
    args: &[OsString],
    request: &str,
) -> std::result::Result<String, String> {
    run_judge_command_within(
        program,
        args,
        request,
        Duration::from_secs(JUDGE_TIMEOUT_SECONDS),
    )
}

fn run_judge_command_within(
    program: &OsStr,
    args: &[OsString],
    request: &str,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let display = Path::new(program).display();
    // The child leads a new process group, so the deadline below
    // can stop the whole group and not only the judge itself.
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|exc| format!("the judge command {display} could not be started, {exc}"))?;
    let pid = child.id();
    let group_guard = crate::ActiveChildGuard::new(pid);
    let deadline = Instant::now() + timeout;
    let mut stdin = child.stdin.take().unwrap();
    let payload = request.as_bytes().to_vec();
    let (writer_out, writer_in) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let written = stdin
            .write_all(&payload)
            .and_then(|()| stdin.write_all(b"\n"));
        drop(stdin);
        let _ = writer_out.send(written);
    });
    let mut stdout = child.stdout.take().unwrap();
    let (reader_out, reader_in) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.read_to_end(&mut bytes);
        let _ = reader_out.send((read, bytes));
    });
    let status = match crate::await_child(child, timeout, &group_guard) {
        crate::ChildWait::Exited(status) => status,
        crate::ChildWait::TimedOut => {
            return Err(format!(
                "the judge command {display} did not finish inside the deadline of {} seconds",
                timeout.as_secs()
            ));
        }
        crate::ChildWait::Failed(exc) => {
            return Err(format!(
                "the judge command {display} could not be waited on, {exc}"
            ));
        }
    };
    if !status.success() {
        return Err(format!("the judge command {display} exited with {status}"));
    }
    // The command answered, so the request write and the reply
    // read are collected over channels against the same deadline.
    // A descendant of the command can hold a pipe open past the
    // command's own exit, and a collection without the bound would
    // wait on that descendant without limit, so a collection that
    // runs out of the time that stands is the deadline expiring,
    // judged exactly as the command running past it is judged.
    // Judged the same way means stopped the same way too: a
    // collection that times out stops the process group before
    // it returns, so the descendant holding the pipe does not
    // survive the operation that disowned it.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match writer_in.recv_timeout(remaining) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(format!(
                "the request could not be written to the judge command {display}"
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            crate::kill_group(pid);
            return Err(format!(
                "the judge command {display} did not finish inside the deadline of {} seconds",
                timeout.as_secs()
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err("the judge writer thread failed".to_string());
        }
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (read, bytes) = match reader_in.recv_timeout(remaining) {
        Ok(carried) => carried,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            crate::kill_group(pid);
            return Err(format!(
                "the judge command {display} did not finish inside the deadline of {} seconds",
                timeout.as_secs()
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err("the judge reader thread failed".to_string());
        }
    };
    read.map_err(|exc| {
        format!("the reply of the judge command {display} could not be read, {exc}")
    })?;
    String::from_utf8(bytes)
        .map_err(|_| format!("the reply of the judge command {display} is not text"))
}

/// The wait bound a parsed claim carries, in the seconds the
/// library takes. The parser refuses a bound past the largest
/// the library names, so no bound reaching here can fail the
/// conversion.
fn wait_bound(parsed: &Args) -> Option<i64> {
    parsed
        .wait_for
        .map(|value| i64::try_from(value).unwrap_or_else(|_| unreachable!()))
}

/// Run one claim of whichever shape its options name, a plain claim
/// by default, a profiled claim under --profile with no
/// consultation, and a routed claim under --describe through the
/// judge command beside it or through no transport at all. A claim
/// carrying --wait takes the waiting shape of its kind, allocating
/// exactly as without the option where its draw stands free and
/// recording a wait where it does not.
fn claim_from_args(stack: &Path, parsed: &Args, out: &mut dyn Write) -> Result<()> {
    let agent = parsed.agent.as_deref().unwrap();
    let transport = parsed
        .judge_command
        .as_ref()
        .map(|program| SubprocessTransport {
            program: program.clone(),
            args: parsed.judge_args.clone(),
        });
    let transport_ref = transport
        .as_ref()
        .map(|transport| transport as &dyn JudgeTransport);
    if let Some(name) = &parsed.profile {
        let profile = profile_named(name)?;
        if parsed.wait {
            let outcome =
                claim_profile_waiting(stack, agent, profile, wait_bound(parsed), transport_ref)?;
            print_outcome(out, &outcome).map_err(output_error)?;
            return Ok(());
        }
        let record = claim_profile(stack, agent, profile)?;
        print_claim(out, &record).map_err(output_error)?;
        return Ok(());
    }
    if let Some(description) = &parsed.describe {
        let default = profile_named(parsed.default_profile.as_deref().unwrap())?;
        if parsed.wait {
            let outcome = claim_routed_waiting(
                stack,
                agent,
                description,
                default,
                wait_bound(parsed),
                transport_ref,
            )?;
            print_outcome(out, &outcome).map_err(output_error)?;
            return Ok(());
        }
        let record = claim_routed(stack, agent, description, default, transport_ref)?;
        print_claim(out, &record).map_err(output_error)?;
        return Ok(());
    }
    if parsed.wait {
        let outcome = claim_waiting(stack, agent, wait_bound(parsed), transport_ref)?;
        print_outcome(out, &outcome).map_err(output_error)?;
        return Ok(());
    }
    let record = claim(stack, agent)?;
    print_claim(out, &record).map_err(output_error)?;
    Ok(())
}

fn print_outcome(out: &mut dyn Write, outcome: &ClaimOutcome) -> std::io::Result<()> {
    match outcome {
        ClaimOutcome::Claim(record) => print_claim(out, record),
        ClaimOutcome::Wait(wait) => print_wait(out, wait),
    }
}

fn print_claim(out: &mut dyn Write, record: &Record) -> std::io::Result<()> {
    writeln!(out, "claimed {}", record.agent)?;
    writeln!(out, "branch {}", record.branch)?;
    writeln!(out, "worktree {}", record.worktree)?;
    writeln!(out, "ports {}-{}", record.port_start, record.port_end)?;
    writeln!(out, "database {}", record.database)?;
    writeln!(out, "env file {}", record.env_file)?;
    Ok(())
}

/// Print a claim that waited. The claim holds no branch and no
/// worktree and no ports, so the print names the wait and the
/// settled profile and the score the consultation returned or
/// the fallback class that left the wait scoreless.
fn print_wait(out: &mut dyn Write, wait: &Wait) -> std::io::Result<()> {
    writeln!(out, "waiting {}", wait.agent)?;
    writeln!(out, "profile {}", wait.profile)?;
    match (wait.score, wait.confidence) {
        (Some(score), Some(confidence)) => {
            writeln!(out, "score {score}")?;
            writeln!(out, "confidence {confidence}")?;
        }
        _ => {
            writeln!(out, "score none")?;
            writeln!(out, "fallback {}", wait.fallback)?;
        }
    }
    if let Some(bound) = wait.wait_for {
        writeln!(out, "wait bound {bound} seconds")?;
    }
    Ok(())
}

fn print_table(out: &mut dyn Write, records: &[Record]) -> std::io::Result<()> {
    let header: Vec<String> = ["AGENT", "BRANCH", "PORTS", "DATABASE", "WORKTREE"]
        .iter()
        .map(|cell| cell.to_string())
        .collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            vec![
                record.agent.clone(),
                record.branch.clone(),
                format!("{}-{}", record.port_start, record.port_end),
                record.database.clone(),
                record.worktree.clone(),
            ]
        })
        .collect();
    print_grid(out, &header, &rows)
}

/// Print the standing waits in offer order, the queue table of
/// obligation SY-24, carrying the agent and the settled profile
/// and the score or the word none and the instant the wait was
/// recorded.
fn print_queue_table(out: &mut dyn Write, waits: &[Wait]) -> std::io::Result<()> {
    let header: Vec<String> = ["AGENT", "PROFILE", "SCORE", "RECORDED"]
        .iter()
        .map(|cell| cell.to_string())
        .collect();
    let rows: Vec<Vec<String>> = waits
        .iter()
        .map(|wait| {
            vec![
                wait.agent.clone(),
                wait.profile.clone(),
                match wait.score {
                    Some(score) => score.to_string(),
                    None => "none".to_string(),
                },
                wait.recorded_at.clone(),
            ]
        })
        .collect();
    print_grid(out, &header, &rows)
}

/// Render one table in the shape the claims table landed, each
/// column as wide as its widest cell and the columns joined by
/// two spaces.
fn print_grid(out: &mut dyn Write, header: &[String], rows: &[Vec<String>]) -> std::io::Result<()> {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let render = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let padding = widths[index] - cell.chars().count();
                format!("{cell}{}", " ".repeat(padding))
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    writeln!(out, "{}", render(header))?;
    for row in rows {
        writeln!(out, "{}", render(row))?;
    }
    Ok(())
}

fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match parse(args, out, err) {
        Ok(Parsed::Run(parsed)) => parsed,
        Ok(Parsed::Exit(code)) => return code,
        Err(message) => {
            let _ = writeln!(
                err,
                "usage: switchyard [-h] {{init,claim,list,release}} ..."
            );
            let _ = writeln!(err, "switchyard: error: {message}");
            return 2;
        }
    };
    let stack = resolve_path(&parsed.stack);
    let result = match parsed.command.as_str() {
        "init" => init_stack(
            &stack,
            parsed.port_start,
            parsed.range_size,
            parsed.range_count,
        )
        .and_then(|pool| {
            writeln!(out, "initialised stack at {}", stack.display()).map_err(output_error)?;
            writeln!(
                out,
                "port pool {} ranges of {} ports from {}",
                pool.range_count, pool.range_size, pool.start
            )
            .map_err(output_error)
        }),
        "claim" => claim_from_args(&stack, &parsed, out),
        "list" => {
            if parsed.queue {
                list_waits(&stack)
                    .and_then(|waits| print_queue_table(out, &waits).map_err(output_error))
            } else {
                list_claims(&stack)
                    .and_then(|records| print_table(out, &records).map_err(output_error))
            }
        }
        "release" => release_report(&stack, parsed.agent.as_deref().unwrap()).and_then(|report| {
            match &report.outcome {
                ReleaseOutcome::Claim(record) => {
                    writeln!(out, "released {}", record.agent).map_err(output_error)?;
                    writeln!(
                        out,
                        "ports {}-{} returned",
                        record.port_start, record.port_end
                    )
                    .map_err(output_error)?;
                }
                ReleaseOutcome::Wait(wait) => {
                    writeln!(out, "released {}", wait.agent).map_err(output_error)?;
                    writeln!(out, "wait cancelled").map_err(output_error)?;
                }
            }
            for failure in &report.furnishing_failures {
                writeln!(
                    err,
                    "note: the wait of agent {} was skipped, furnishing it failed, {}",
                    failure.agent, failure.message
                )
                .map_err(output_error)?;
            }
            Ok(())
        }),
        _ => unreachable!(),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "error: {}", error.message());
            error.exit_code()
        }
    }
}

/// Forward one signal to the group of the child standing inside
/// this command, where one stands.
///
/// A spawned child leads its own process group, so the
/// terminal's interrupt never reaches it, and a child whose
/// parent died by the signal would run on with nothing left to
/// enforce its deadline. The runners record the standing
/// child's group in the crate registry; this sends the signal
/// there, so the child and its descendants take the same signal
/// the operator sent the command.
fn forward_to_active_child(signal: i32) {
    let group = active_child_group();
    if group > 0 {
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

/// The signal handler the command installs. It forwards the
/// signal to the standing child's group, then restores the
/// default disposition and raises the signal again, so the
/// command itself still ends the way the operator asked. The
/// handler confines itself to an atomic load, kill, signal, and
/// raise, the calls a signal handler may make.
extern "C" fn forward_signal(signal: i32) {
    forward_to_active_child(signal);
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Install the forwarding handler for the two signals an
/// operator or a supervisor ends a command with.
fn install_signal_forwarding() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            forward_signal as *const () as libc::sighandler_t,
        );
    }
}

pub fn main() -> i32 {
    install_signal_forwarding();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let code = run(&args, &mut out, &mut err);
    match (code, out.flush()) {
        (0, Err(exc)) => {
            let _ = writeln!(err, "error: {}", output_error(exc).message());
            let _ = err.flush();
            1
        }
        _ => {
            let _ = err.flush();
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_judge_that_times_out_takes_its_descendants_with_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "switchyard-cli-test-{}-judge-descendant",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let heartbeat = dir.join("heartbeat");
        // The judge backgrounds a loop that appends to a
        // heartbeat file five times a second, and then sleeps in
        // the foreground, so the deadline stops the judge while
        // its descendant stands. A kill of the judge alone leaves
        // the loop appending, and this test fails against it; the
        // group kill stops the loop, and the heartbeat stands
        // still. The script is staged under a temporary name and
        // renamed into place, the way the library's tests stage
        // their fake git, so the spawn can never race the writer
        // still holding the file open.
        let staged = dir.join("judge.staged");
        std::fs::write(
            &staged,
            format!(
                "#!/bin/sh\nwhile true; do echo tick >> {}; sleep 0.2; done &\nsleep 30\n",
                heartbeat.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = dir.join("judge.sh");
        std::fs::rename(&staged, &program).unwrap();
        let result =
            run_judge_command_within(&program.to_string_lossy(), "{}", Duration::from_secs(1));
        let error = result.expect_err("the sleeping judge must pass its deadline");
        assert!(
            error.contains("did not finish inside the deadline of 1 seconds"),
            "{error}"
        );
        std::thread::sleep(Duration::from_millis(300));
        let settled = std::fs::metadata(&heartbeat).unwrap().len();
        std::thread::sleep(Duration::from_millis(1500));
        let later = std::fs::metadata(&heartbeat).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            settled, later,
            "the descendant of the timed out judge kept running"
        );
    }

    /// A writer whose every write fails, standing in for a
    /// closed pipe and a full device at the command's output.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the output is full in this test"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The checked prints of SY-A33. A claim whose every output
    /// write fails still lands, because the allocation does not
    /// depend on its report, and the command exits 1 with the
    /// output refusal on its error stream. On the code before
    /// the check the writes were discarded and this run exited
    /// 0, so the pin fails there for the real reason.
    #[test]
    fn a_claim_whose_output_cannot_be_written_fails_loudly_and_still_lands() {
        let dir = std::env::temp_dir().join(format!(
            "switchyard-cli-test-{}-output-fails",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stack = dir.join("stack");
        crate::init_stack(&stack, 20000, 10, 2).unwrap();
        let args: Vec<OsString> = vec![
            "claim".into(),
            "alpha".into(),
            "--stack".into(),
            stack.as_os_str().to_os_string(),
        ];
        let mut err: Vec<u8> = Vec::new();
        let code = run(&args, &mut FailingWriter, &mut err);
        assert_eq!(code, 1);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("the command's output could not be written"),
            "{text}"
        );
        let records = crate::list_claims(&stack).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent, "alpha");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The help print is checked the same way: a help write
    /// that fails exits 1 with the refusal on the error stream,
    /// where the discarded write exited 0.
    #[test]
    fn a_help_whose_output_cannot_be_written_fails_loudly() {
        let args: Vec<OsString> = vec!["--help".into()];
        let mut err: Vec<u8> = Vec::new();
        let code = run(&args, &mut FailingWriter, &mut err);
        assert_eq!(code, 1);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("the command's output could not be written"),
            "{text}"
        );
    }

    #[test]
    fn a_judge_that_exits_early_leaves_no_descendant_on_the_pipe() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "switchyard-cli-test-{}-judge-early-exit",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let heartbeat = dir.join("heartbeat");
        // The judge backgrounds a loop that appends to a
        // heartbeat file five times a second, and then exits at
        // once with success. The loop inherits the reply pipe
        // and holds it open past the judge's own exit, so the
        // reply collection runs out of the deadline even though
        // the judge answered in time. A collection timeout that
        // only returned the error would leave the loop standing
        // on the pipe, and the heartbeat would keep growing;
        // the group stop at the collection deadline stops it.
        // The judge consumes its standard input before it exits,
        // the way the integration suite's judge of the same shape
        // does, because a judge that exits before the request
        // write lands fails the write instead, and the runner
        // judges the write first, so the consultation would
        // return the write error and race the deadline outcome
        // this pin asserts. The script is staged and renamed
        // into place, as the test above stages its own.
        let staged = dir.join("judge.staged");
        std::fs::write(
            &staged,
            format!(
                "#!/bin/sh\nwhile true; do echo tick >> {}; sleep 0.2; done &\ncat > /dev/null\nexit 0\n",
                heartbeat.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = dir.join("judge.sh");
        std::fs::rename(&staged, &program).unwrap();
        let result =
            run_judge_command_within(&program.to_string_lossy(), "{}", Duration::from_secs(1));
        let error = result.expect_err("the held reply pipe must pass the collection deadline");
        assert!(
            error.contains("did not finish inside the deadline of 1 seconds"),
            "{error}"
        );
        std::thread::sleep(Duration::from_millis(300));
        let settled = std::fs::metadata(&heartbeat).unwrap().len();
        std::thread::sleep(Duration::from_millis(1500));
        let later = std::fs::metadata(&heartbeat).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            settled, later,
            "the descendant of the exited judge kept running"
        );
    }

    #[test]
    fn a_forwarded_signal_stops_the_standing_child_and_its_descendants() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "switchyard-cli-test-{}-forwarded-signal",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let heartbeat = dir.join("heartbeat");
        // A child leading its own group, backgrounding the same
        // heartbeat loop the descendant pins stage and then
        // sleeping in the foreground, recorded in the forwarding
        // registry the way a runner records its child. The
        // forward must reach the whole group: a forward aimed
        // only at the child leaves the loop appending, and this
        // test fails against it. The script is staged and renamed
        // into place, as the tests above stage theirs.
        let staged = dir.join("child.staged");
        std::fs::write(
            &staged,
            format!(
                "#!/bin/sh\nwhile true; do echo tick >> {}; sleep 0.2; done &\nsleep 30\n",
                heartbeat.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = dir.join("child.sh");
        std::fs::rename(&staged, &program).unwrap();
        let mut child = Command::new(&program).process_group(0).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        crate::note_child_spawned(child.id());
        forward_to_active_child(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        let stopped = loop {
            match child.try_wait().unwrap() {
                Some(_) => break true,
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => break false,
            }
        };
        crate::note_child_finished(child.id());
        assert!(stopped, "the forwarded signal did not stop the child");
        std::thread::sleep(Duration::from_millis(300));
        let settled = std::fs::metadata(&heartbeat).unwrap().len();
        std::thread::sleep(Duration::from_millis(1500));
        let later = std::fs::metadata(&heartbeat).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            settled, later,
            "the descendant of the signalled child kept running"
        );
    }
}
