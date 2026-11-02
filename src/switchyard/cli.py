"""Command line interface for switchyard.

One stack per directory, four commands. Init creates the stack, claim
allocates, list shows who holds what, and release returns everything.
"""

import argparse
import sys
from pathlib import Path

from . import core


def build_parser():
    parser = argparse.ArgumentParser(
        prog="switchyard",
        description="Give every parallel agent its own branch, port range, "
        "database, and env file with one command.",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    init = commands.add_parser(
        "init", help="create a stack root with a repository and a registry"
    )
    init.add_argument("--stack", required=True, type=Path)
    init.add_argument("--port-start", type=int, default=core.DEFAULT_PORT_START)
    init.add_argument("--range-size", type=int, default=core.DEFAULT_RANGE_SIZE)
    init.add_argument("--range-count", type=int, default=core.DEFAULT_RANGE_COUNT)

    claim = commands.add_parser(
        "claim", help="allocate a worktree, a range, a database, and an env file"
    )
    claim.add_argument("agent")
    claim.add_argument("--stack", required=True, type=Path)

    listing = commands.add_parser(
        "list", help="print who holds what, derived only from the registry"
    )
    listing.add_argument("--stack", required=True, type=Path)

    release = commands.add_parser(
        "release", help="return every resource an agent holds"
    )
    release.add_argument("agent")
    release.add_argument("--stack", required=True, type=Path)

    return parser


def _print_claim(record):
    print(f"claimed {record['agent']}")
    print(f"branch {record['branch']}")
    print(f"worktree {record['worktree']}")
    print(f"ports {record['port_start']}-{record['port_end']}")
    print(f"database {record['database']}")
    print(f"env file {record['env_file']}")


def _print_table(records):
    header = ("AGENT", "BRANCH", "PORTS", "DATABASE", "WORKTREE")
    rows = [
        (
            record["agent"],
            record["branch"],
            f"{record['port_start']}-{record['port_end']}",
            record["database"],
            record["worktree"],
        )
        for record in records
    ]
    widths = [len(cell) for cell in header]
    for row in rows:
        for index, cell in enumerate(row):
            widths[index] = max(widths[index], len(cell))
    for row in [header, *rows]:
        print("  ".join(cell.ljust(width) for cell, width in zip(row, widths)))


def main(argv=None):
    args = build_parser().parse_args(argv)
    args.stack = args.stack.resolve()
    try:
        if args.command == "init":
            pool = core.init_stack(
                args.stack, args.port_start, args.range_size, args.range_count
            )
            print(f"initialised stack at {args.stack}")
            print(
                f"port pool {pool['range_count']} ranges of {pool['range_size']} "
                f"ports from {pool['start']}"
            )
        elif args.command == "claim":
            _print_claim(core.claim(args.stack, args.agent))
        elif args.command == "list":
            _print_table(core.list_claims(args.stack))
        elif args.command == "release":
            record = core.release(args.stack, args.agent)
            print(f"released {record['agent']}")
            print(f"ports {record['port_start']}-{record['port_end']} returned")
    except core.SwitchyardError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return exc.exit_code
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
