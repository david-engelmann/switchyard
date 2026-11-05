//! Command line interface for switchyard.
//!
//! One stack per directory, four commands. Init creates the stack,
//! claim allocates, list shows who holds what, and release returns
//! everything. A claim is plain, or profiled under a declared
//! profile, or routed by a job description a judge settles.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{
    claim, claim_profile, claim_routed, init_stack, list_claims, profile_named, release,
    resolve_path, JudgeTransport, Record, Result, DEFAULT_PORT_START, DEFAULT_RANGE_COUNT,
    DEFAULT_RANGE_SIZE, JUDGE_TIMEOUT_SECONDS,
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

claim options:
  --profile NAME           allocate under a declared profile, one of
                           lean, standard, or roomy, with no consultation
  --describe TEXT          route the claim by a job description the
                           pinned decision model judges
  --default-profile NAME   the profile a routed claim lands on when the
                           consultation does not decide, required
                           beside --describe
  --judge-command PROGRAM  the one program a routed claim consults,
                           named beside --describe

A judge command is spawned with no arguments. It receives the request
document as one line on its standard input and answers with the reply
document on its standard output, inside a deadline of 30 seconds. A
judge command that cannot be started, or whose request cannot be
written, or that answers nothing usable, or that runs past the
deadline leaves the claim on its declared default, and the routing
log records the landing.
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
    judge_command: Option<String>,
}

enum Parsed {
    Run(Box<Args>),
    Exit(i32),
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

fn parse(args: &[OsString], out: &mut dyn Write) -> std::result::Result<Parsed, String> {
    let tokens: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let first = match tokens.first() {
        Some(token) => token.clone(),
        None => return Err("the following arguments are required: command".to_string()),
    };
    if first == "-h" || first == "--help" {
        let _ = write!(out, "{HELP}");
        return Ok(Parsed::Exit(0));
    }
    if !COMMANDS.contains(&first.as_str()) {
        if first.starts_with('-') {
            return Err(format!("unrecognized arguments: {first}"));
        }
        return Err(format!(
            "invalid choice: '{first}' (choose from 'init', 'claim', 'list', 'release')"
        ));
    }
    let command = first;
    let mut agent: Option<String> = None;
    let mut stack: Option<PathBuf> = None;
    let mut port_start: i128 = DEFAULT_PORT_START as i128;
    let mut range_size: i128 = DEFAULT_RANGE_SIZE as i128;
    let mut range_count: i128 = DEFAULT_RANGE_COUNT as i128;
    let mut describe: Option<String> = None;
    let mut default_profile: Option<String> = None;
    let mut profile: Option<String> = None;
    let mut judge_command: Option<String> = None;
    let mut index = 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "-h" || token == "--help" {
            let _ = write!(out, "{HELP}");
            return Ok(Parsed::Exit(0));
        }
        let (option, inline_value) = match token.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_string(), Some(value.to_string()))
            }
            _ => (token.clone(), None),
        };
        let known_option = matches!(
            option.as_str(),
            "--stack" | "--port-start" | "--range-size" | "--range-count"
        ) && (option == "--stack" || command == "init")
            || matches!(
                option.as_str(),
                "--describe" | "--default-profile" | "--profile" | "--judge-command"
            ) && command == "claim";
        if known_option {
            let value = match inline_value {
                Some(value) => value,
                None => {
                    let next = tokens.get(index + 1);
                    match next {
                        Some(value) if !value.starts_with("--") => {
                            index += 1;
                            value.clone()
                        }
                        _ => return Err(format!("argument {option}: expected one argument")),
                    }
                }
            };
            match option.as_str() {
                "--stack" => stack = Some(PathBuf::from(value)),
                "--port-start" => {
                    port_start = match parse_int(&value) {
                        Some(number) => number,
                        None => {
                            return Err(format!(
                                "argument --port-start: invalid int value: '{value}'"
                            ))
                        }
                    }
                }
                "--range-size" => {
                    range_size = match parse_int(&value) {
                        Some(number) => number,
                        None => {
                            return Err(format!(
                                "argument --range-size: invalid int value: '{value}'"
                            ))
                        }
                    }
                }
                "--range-count" => {
                    range_count = match parse_int(&value) {
                        Some(number) => number,
                        None => {
                            return Err(format!(
                                "argument --range-count: invalid int value: '{value}'"
                            ))
                        }
                    }
                }
                "--describe" => describe = Some(value),
                "--default-profile" => default_profile = Some(value),
                "--profile" => profile = Some(value),
                "--judge-command" => judge_command = Some(value),
                _ => unreachable!(),
            }
            index += 1;
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            return Err(format!("unrecognized arguments: {token}"));
        }
        if (command == "claim" || command == "release") && agent.is_none() {
            agent = Some(token.clone());
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
        if describe.is_none() && judge_command.is_some() {
            return Err(
                "argument --judge-command: a routed claim takes --describe beside it".to_string(),
            );
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
    })))
}

/// The transport that consults a judge through one named program,
/// the operator's side of the seam of obligation SY-16.
///
/// The program is spawned with no arguments, receives the request
/// document as one line on its standard input, and answers with
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
    program: String,
}

impl JudgeTransport for SubprocessTransport {
    fn consult(&self, request: &str) -> std::result::Result<String, String> {
        run_judge_command(&self.program, request)
    }
}

fn run_judge_command(program: &str, request: &str) -> std::result::Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(JUDGE_TIMEOUT_SECONDS);
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|exc| format!("the judge command {program} could not be started, {exc}"))?;
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
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "the judge command {program} did not finish inside the deadline of {JUDGE_TIMEOUT_SECONDS} seconds"
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(exc) => {
                return Err(format!(
                    "the judge command {program} could not be waited on, {exc}"
                ));
            }
        }
    };
    if !status.success() {
        return Err(format!("the judge command {program} exited with {status}"));
    }
    // The command answered, so the request write and the reply
    // read are collected over channels against the same deadline.
    // A descendant of the command can hold a pipe open past the
    // command's own exit, and a collection without the bound would
    // wait on that descendant without limit, so a collection that
    // runs out of the time that stands is the deadline expiring,
    // judged exactly as the command running past it is judged.
    let remaining = deadline.saturating_duration_since(Instant::now());
    match writer_in.recv_timeout(remaining) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(format!(
                "the request could not be written to the judge command {program}"
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(format!(
                "the judge command {program} did not finish inside the deadline of {JUDGE_TIMEOUT_SECONDS} seconds"
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
            return Err(format!(
                "the judge command {program} did not finish inside the deadline of {JUDGE_TIMEOUT_SECONDS} seconds"
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err("the judge reader thread failed".to_string());
        }
    };
    read.map_err(|exc| {
        format!("the reply of the judge command {program} could not be read, {exc}")
    })?;
    String::from_utf8(bytes)
        .map_err(|_| format!("the reply of the judge command {program} is not text"))
}

/// Run one claim of whichever shape its options name, a plain claim
/// by default, a profiled claim under --profile with no
/// consultation, and a routed claim under --describe through the
/// judge command beside it or through no transport at all.
fn claim_from_args(stack: &Path, parsed: &Args) -> Result<Record> {
    let agent = parsed.agent.as_deref().unwrap();
    if let Some(name) = &parsed.profile {
        let profile = profile_named(name)?;
        return claim_profile(stack, agent, profile);
    }
    if let Some(description) = &parsed.describe {
        let default = profile_named(parsed.default_profile.as_deref().unwrap())?;
        let transport = parsed
            .judge_command
            .as_ref()
            .map(|program| SubprocessTransport {
                program: program.clone(),
            });
        return claim_routed(
            stack,
            agent,
            description,
            default,
            transport
                .as_ref()
                .map(|transport| transport as &dyn JudgeTransport),
        );
    }
    claim(stack, agent)
}

fn print_claim(out: &mut dyn Write, record: &Record) {
    let _ = writeln!(out, "claimed {}", record.agent);
    let _ = writeln!(out, "branch {}", record.branch);
    let _ = writeln!(out, "worktree {}", record.worktree);
    let _ = writeln!(out, "ports {}-{}", record.port_start, record.port_end);
    let _ = writeln!(out, "database {}", record.database);
    let _ = writeln!(out, "env file {}", record.env_file);
}

fn print_table(out: &mut dyn Write, records: &[Record]) {
    let header: [String; 5] = [
        "AGENT".to_string(),
        "BRANCH".to_string(),
        "PORTS".to_string(),
        "DATABASE".to_string(),
        "WORKTREE".to_string(),
    ];
    let rows: Vec<[String; 5]> = records
        .iter()
        .map(|record| {
            [
                record.agent.clone(),
                record.branch.clone(),
                format!("{}-{}", record.port_start, record.port_end),
                record.database.clone(),
                record.worktree.clone(),
            ]
        })
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let render = |cells: &[String; 5]| -> String {
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
    let _ = writeln!(out, "{}", render(&header));
    for row in &rows {
        let _ = writeln!(out, "{}", render(row));
    }
}

fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match parse(args, out) {
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
        .map(|pool| {
            let _ = writeln!(out, "initialised stack at {}", stack.display());
            let _ = writeln!(
                out,
                "port pool {} ranges of {} ports from {}",
                pool.range_count, pool.range_size, pool.start
            );
        }),
        "claim" => claim_from_args(&stack, &parsed).map(|record| {
            print_claim(out, &record);
        }),
        "list" => list_claims(&stack).map(|records| {
            print_table(out, &records);
        }),
        "release" => release(&stack, parsed.agent.as_deref().unwrap()).map(|record| {
            let _ = writeln!(out, "released {}", record.agent);
            let _ = writeln!(
                out,
                "ports {}-{} returned",
                record.port_start, record.port_end
            );
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

pub fn main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let code = run(&args, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}
