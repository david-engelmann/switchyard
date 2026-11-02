"""Core allocation logic for switchyard.

A stack is a plain directory. Inside it live a git repository, a
worktrees directory, a lock file, and one registry file that records
the port pool and every live claim. The registry is the only record,
and every mutating command holds an exclusive flock lock on the lock
file across the whole read and check and modify and write of the
registry, so racing processes cannot both take the same agent name or
overlapping port ranges.
"""

import fcntl
import json
import os
import re
import subprocess
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

REGISTRY_NAME = "switchyard-registry.json"
LOCK_NAME = "switchyard.lock"
REPO_NAME = "repo"
WORKTREES_NAME = "worktrees"
ENV_NAME = ".env.switchyard"
REGISTRY_VERSION = 1

DEFAULT_PORT_START = 20000
DEFAULT_RANGE_SIZE = 10
DEFAULT_RANGE_COUNT = 16

AGENT_NAME_PATTERN = re.compile(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?$")
AGENT_NAME_MAX = 40


class SwitchyardError(Exception):
    """An error the CLI reports with a nonzero exit and no traceback."""

    exit_code = 1


class NotInitialisedError(SwitchyardError):
    """The stack root holds no registry, so nothing is recorded there."""

    exit_code = 2


class CorruptRegistryError(SwitchyardError):
    """The registry cannot be parsed or its shape is wrong."""

    exit_code = 2


class InvalidAgentNameError(SwitchyardError):
    """The agent name is not one path safe component."""

    exit_code = 2


def validate_agent_name(agent):
    if (
        not isinstance(agent, str)
        or len(agent) > AGENT_NAME_MAX
        or not AGENT_NAME_PATTERN.match(agent)
    ):
        raise InvalidAgentNameError(
            f"invalid agent name {agent!r}, use lowercase letters and digits "
            "and hyphens as one path safe component"
        )


def branch_name(agent):
    return f"switchyard/{agent}"


def database_name(agent):
    return "switchyard_" + agent.replace("-", "_")


def _registry_path(stack):
    return stack / REGISTRY_NAME


def _is_int(value):
    return type(value) is int


def _corrupt(path, why):
    return CorruptRegistryError(f"registry at {path} is corrupt, {why}")


def _validate_registry(data, path):
    if not isinstance(data, dict):
        raise _corrupt(path, "the top level is not an object")
    if data.get("version") != REGISTRY_VERSION:
        raise _corrupt(path, "the version is missing or unsupported")
    pool = data.get("port_pool")
    if not isinstance(pool, dict):
        raise _corrupt(path, "the port pool is missing")
    for key in ("start", "range_size", "range_count"):
        if not _is_int(pool.get(key)) or pool[key] <= 0:
            raise _corrupt(path, f"the port pool field {key} is not a positive integer")
    pool_end = pool["start"] + pool["range_size"] * pool["range_count"] - 1
    claims = data.get("claims")
    if not isinstance(claims, dict):
        raise _corrupt(path, "the claims are missing or not an object")
    ranges = []
    for agent, record in claims.items():
        if not isinstance(record, dict):
            raise _corrupt(path, f"the claim for {agent} is not an object")
        if record.get("agent") != agent:
            raise _corrupt(path, f"the claim keyed {agent} names a different agent")
        for key in ("branch", "worktree", "database", "env_file"):
            if not isinstance(record.get(key), str) or not record[key]:
                raise _corrupt(path, f"the claim for {agent} has no sound {key}")
        for key in ("port_start", "port_end"):
            if not _is_int(record.get(key)):
                raise _corrupt(path, f"the claim for {agent} has no sound {key}")
        if record["port_start"] > record["port_end"]:
            raise _corrupt(path, f"the claim for {agent} has an inverted port range")
        if record["port_start"] < pool["start"] or record["port_end"] > pool_end:
            raise _corrupt(path, f"the claim for {agent} reaches outside the port pool")
        ranges.append((record["port_start"], record["port_end"], agent))
    ranges.sort()
    for (prev_start, prev_end, prev_agent), (start, end, agent) in zip(
        ranges, ranges[1:]
    ):
        if start <= prev_end:
            raise _corrupt(
                path,
                f"the claims for {prev_agent} and {agent} hold overlapping ranges",
            )
    return data


def load_registry(stack):
    """Read and validate the registry, changing nothing on disk."""
    path = _registry_path(stack)
    if not path.is_file():
        raise NotInitialisedError(
            f"no registry at {path}, run switchyard init to create the stack"
        )
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise _corrupt(path, f"it cannot be read, {exc}") from exc
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise _corrupt(path, f"it is not valid JSON, {exc}") from exc
    return _validate_registry(data, path)


def _save_registry(stack, data):
    path = _registry_path(stack)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


@contextmanager
def _locked(stack):
    lock_path = stack / LOCK_NAME
    try:
        fd = os.open(lock_path, os.O_RDWR)
    except FileNotFoundError as exc:
        raise NotInitialisedError(
            f"no lock file at {lock_path}, run switchyard init to create the stack"
        ) from exc
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _git(repo, *args):
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise SwitchyardError(
            f"git {' '.join(args)} failed with exit {result.returncode}, {detail}"
        )
    return result.stdout


def _git_quiet(repo, *args):
    subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
    )


def init_stack(stack, port_start, range_size, range_count):
    """Create a stack root with a repository, a lock file, and a registry."""
    stack = Path(stack)
    if _registry_path(stack).is_file():
        raise SwitchyardError(f"stack at {stack} is already initialised")
    if not _is_int(port_start) or port_start <= 0:
        raise SwitchyardError("the port start must be a positive integer")
    if not _is_int(range_size) or range_size <= 0:
        raise SwitchyardError("the range size must be a positive integer")
    if not _is_int(range_count) or range_count <= 0:
        raise SwitchyardError("the range count must be a positive integer")
    if port_start + range_size * range_count - 1 > 65535:
        raise SwitchyardError("the port pool reaches past port 65535")
    repo = stack / REPO_NAME
    (stack / WORKTREES_NAME).mkdir(parents=True, exist_ok=True)
    repo.mkdir(parents=True, exist_ok=True)
    _git(stack, "init", "-q", "-b", "main", str(repo))
    _git(repo, "config", "user.email", "switchyard@example.invalid")
    _git(repo, "config", "user.name", "switchyard")
    (repo / "README.md").write_text(
        "Synthetic stack repository created by switchyard init.\n",
        encoding="utf-8",
    )
    _git(repo, "add", "README.md")
    _git(repo, "commit", "-q", "-m", "Initialise the stack repository")
    (stack / LOCK_NAME).touch()
    data = {
        "version": REGISTRY_VERSION,
        "port_pool": {
            "start": port_start,
            "range_size": range_size,
            "range_count": range_count,
        },
        "claims": {},
    }
    _save_registry(stack, data)
    return data["port_pool"]


def _free_range(data):
    pool = data["port_pool"]
    taken = [
        (record["port_start"], record["port_end"])
        for record in data["claims"].values()
    ]
    for index in range(pool["range_count"]):
        start = pool["start"] + index * pool["range_size"]
        end = start + pool["range_size"] - 1
        if all(end < low or start > high for low, high in taken):
            return start, end
    return None


def _env_text(record):
    lines = [
        "# Written by switchyard. This file dies with the worktree.",
        f"SWITCHYARD_AGENT={record['agent']}",
        f"SWITCHYARD_BRANCH={record['branch']}",
        f"SWITCHYARD_WORKTREE={record['worktree']}",
        f"SWITCHYARD_PORT_START={record['port_start']}",
        f"SWITCHYARD_PORT_END={record['port_end']}",
        f"PORT={record['port_start']}",
        f"SWITCHYARD_DATABASE={record['database']}",
        f"DATABASE_URL=postgres://127.0.0.1:{record['port_start']}/{record['database']}",
    ]
    return "\n".join(lines) + "\n"


def claim(stack, agent):
    """Allocate a worktree, a branch, a port range, a database, an env file.

    The whole check and set runs under the stack lock, and the registry
    is loaded only after the lock is held, so a racing process can
    never act on a stale read.
    """
    stack = Path(stack)
    validate_agent_name(agent)
    repo = stack / REPO_NAME
    branch = branch_name(agent)
    worktree_rel = f"{WORKTREES_NAME}/{agent}"
    worktree = stack / worktree_rel
    env_rel = f"{worktree_rel}/{ENV_NAME}"
    with _locked(stack):
        data = load_registry(stack)
        if agent in data["claims"]:
            raise SwitchyardError(
                f"agent {agent} already holds a claim in this stack"
            )
        chosen = _free_range(data)
        if chosen is None:
            raise SwitchyardError(
                "the port pool is exhausted, every range is held"
            )
        port_start, port_end = chosen
        record = {
            "agent": agent,
            "branch": branch,
            "worktree": worktree_rel,
            "port_start": port_start,
            "port_end": port_end,
            "database": database_name(agent),
            "env_file": env_rel,
            "claimed_at": datetime.now(timezone.utc).isoformat(),
        }
        created = False
        try:
            _git(repo, "worktree", "add", "-b", branch, str(worktree))
            created = True
            (stack / env_rel).write_text(_env_text(record), encoding="utf-8")
            data["claims"][agent] = record
            _save_registry(stack, data)
        except BaseException:
            if created:
                _git_quiet(repo, "worktree", "remove", "--force", str(worktree))
                _git_quiet(repo, "branch", "-D", branch)
            data["claims"].pop(agent, None)
            raise
        return record


def release(stack, agent):
    """Return every resource a claim holds and remove its record."""
    stack = Path(stack)
    validate_agent_name(agent)
    repo = stack / REPO_NAME
    with _locked(stack):
        data = load_registry(stack)
        record = data["claims"].get(agent)
        if record is None:
            raise SwitchyardError(f"agent {agent} holds no claim in this stack")
        worktree = stack / record["worktree"]
        if worktree.exists():
            _git(repo, "worktree", "remove", "--force", str(worktree))
        else:
            _git(repo, "worktree", "prune")
        listed = _git(repo, "branch", "--list", record["branch"])
        if listed.strip():
            _git(repo, "branch", "-D", record["branch"])
        del data["claims"][agent]
        _save_registry(stack, data)
        return record


def list_claims(stack):
    """Return the recorded claims sorted by agent, reading nothing else."""
    stack = Path(stack)
    data = load_registry(stack)
    return [data["claims"][agent] for agent in sorted(data["claims"])]
