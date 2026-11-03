//! Command line interface for switchyard.
//!
//! One stack per directory, four commands. Init creates the stack,
//! claim allocates, list shows who holds what, and release returns
//! everything.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::{
    claim, init_stack, list_claims, release, resolve_path, Record, DEFAULT_PORT_START,
    DEFAULT_RANGE_COUNT, DEFAULT_RANGE_SIZE,
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
";

struct Args {
    command: String,
    agent: Option<String>,
    stack: PathBuf,
    port_start: i64,
    range_size: i64,
    range_count: i64,
}

enum Parsed {
    Run(Args),
    Exit(i32),
}

fn parse_int(text: &str) -> Option<i64> {
    let value: i128 = text.parse().ok()?;
    Some(value.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
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
    let mut port_start = DEFAULT_PORT_START;
    let mut range_size = DEFAULT_RANGE_SIZE;
    let mut range_count = DEFAULT_RANGE_COUNT;
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
        ) && (option == "--stack" || command == "init");
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
    Ok(Parsed::Run(Args {
        command,
        agent,
        stack,
        port_start,
        range_size,
        range_count,
    }))
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
        "claim" => claim(&stack, parsed.agent.as_deref().unwrap()).map(|record| {
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
