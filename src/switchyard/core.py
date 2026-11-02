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
import shutil
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
        or not AGENT_NAME_PATTERN.fullmatch(agent)
    ):
        raise InvalidAgentNameError(
            f"invalid agent name {agent!r}, use one path safe component of "
            f"lowercase letters and digits and hyphens, at most "
            f"{AGENT_NAME_MAX} characters long"
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


class _DuplicateKeyError(ValueError):
    """A JSON object in the registry repeated a key."""


def _object_without_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise _DuplicateKeyError(key)
        result[key] = value
    return result


REGISTRY_KEYS = {"version", "port_pool", "claims"}
POOL_KEYS = {"start", "range_size", "range_count"}
RECORD_KEYS = {
    "agent",
    "branch",
    "worktree",
    "port_start",
    "port_end",
    "database",
    "env_file",
    "claimed_at",
}


def _derived_fields(agent):
    worktree = f"{WORKTREES_NAME}/{agent}"
    return {
        "branch": branch_name(agent),
        "worktree": worktree,
        "database": database_name(agent),
        "env_file": f"{worktree}/{ENV_NAME}",
    }


def _validate_registry(data, path):
    if not isinstance(data, dict):
        raise _corrupt(path, "the top level is not an object")
    if set(data) != REGISTRY_KEYS:
        raise _corrupt(
            path,
            "the top level fields are not exactly version and port_pool and claims",
        )
    if not _is_int(data["version"]) or data["version"] != REGISTRY_VERSION:
        raise _corrupt(path, "the version is missing or unsupported")
    pool = data["port_pool"]
    if not isinstance(pool, dict):
        raise _corrupt(path, "the port pool is missing")
    if set(pool) != POOL_KEYS:
        raise _corrupt(
            path,
            "the port pool fields are not exactly start and range_size and range_count",
        )
    for key in ("start", "range_size", "range_count"):
        if not _is_int(pool[key]) or pool[key] <= 0:
            raise _corrupt(path, f"the port pool field {key} is not a positive integer")
    pool_end = pool["start"] + pool["range_size"] * pool["range_count"] - 1
    if pool_end > 65535:
        raise _corrupt(path, "the port pool reaches past port 65535")
    claims = data["claims"]
    if not isinstance(claims, dict):
        raise _corrupt(path, "the claims are missing or not an object")
    ranges = []
    for agent, record in claims.items():
        try:
            validate_agent_name(agent)
        except InvalidAgentNameError:
            raise _corrupt(
                path, f"the claim key {agent} is not a safe agent name"
            ) from None
        if not isinstance(record, dict):
            raise _corrupt(path, f"the claim for {agent} is not an object")
        if set(record) != RECORD_KEYS:
            raise _corrupt(
                path, f"the claim for {agent} does not carry exactly the recorded fields"
            )
        if record["agent"] != agent:
            raise _corrupt(path, f"the claim keyed {agent} names a different agent")
        for key in ("branch", "worktree", "database", "env_file", "claimed_at"):
            if not isinstance(record[key], str) or not record[key]:
                raise _corrupt(path, f"the claim for {agent} has no sound {key}")
        for key, expected in _derived_fields(agent).items():
            if record[key] != expected:
                raise _corrupt(
                    path,
                    f"the claim for {agent} records {key} as {record[key]} "
                    f"where the agent name derives {expected}",
                )
        try:
            datetime.fromisoformat(record["claimed_at"])
        except ValueError:
            raise _corrupt(
                path,
                f"the claim for {agent} has a claimed_at that does not "
                "parse as a timestamp",
            ) from None
        for key in ("port_start", "port_end"):
            if not _is_int(record[key]):
                raise _corrupt(path, f"the claim for {agent} has no sound {key}")
        if record["port_start"] > record["port_end"]:
            raise _corrupt(path, f"the claim for {agent} has an inverted port range")
        if record["port_start"] < pool["start"] or record["port_end"] > pool_end:
            raise _corrupt(path, f"the claim for {agent} reaches outside the port pool")
        if (record["port_start"] - pool["start"]) % pool["range_size"] != 0 or (
            record["port_end"] != record["port_start"] + pool["range_size"] - 1
        ):
            raise _corrupt(
                path,
                f"the claim for {agent} does not hold exactly one slot "
                "of the port pool grid",
            )
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
    if not path.exists():
        raise NotInitialisedError(
            f"no registry at {path}, run switchyard init to create the stack"
        )
    if not path.is_file():
        raise _corrupt(path, "the registry path is not a file")
    try:
        raw = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        raise _corrupt(path, f"it is not valid UTF-8 text, {exc}") from exc
    except OSError as exc:
        raise _corrupt(path, f"it cannot be read, {exc}") from exc
    try:
        data = json.loads(raw, object_pairs_hook=_object_without_duplicate_keys)
    except json.JSONDecodeError as exc:
        raise _corrupt(path, f"it is not valid JSON, {exc}") from exc
    except _DuplicateKeyError as exc:
        raise _corrupt(path, f"it repeats the key {exc}") from exc
    except RecursionError:
        raise _corrupt(path, "it nests too deeply to be read") from None
    return _validate_registry(data, path)


def _save_registry(stack, data):
    path = _registry_path(stack)
    tmp = path.with_name(path.name + ".tmp")
    try:
        tmp.write_text(
            json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        os.replace(tmp, path)
    except OSError as exc:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise SwitchyardError(
            f"the registry at {path} cannot be written, {exc}"
        ) from exc


@contextmanager
def _locked(stack):
    lock_path = stack / LOCK_NAME
    try:
        fd = os.open(lock_path, os.O_RDWR)
    except FileNotFoundError:
        fd = _recreate_lock(stack, lock_path)
    except OSError as exc:
        raise SwitchyardError(
            f"the lock file at {lock_path} cannot be opened, {exc}"
        ) from exc
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _recreate_lock(stack, lock_path):
    """Recreate a missing lock file when the registry survives.

    The lock file is an empty marker whose only job is to be flocked,
    so a stack whose registry is intact but whose lock file was deleted
    recovers by recreating the marker. A stack with no registry is not
    initialised, and nothing is created for it here.
    """
    if not _registry_path(stack).is_file():
        raise NotInitialisedError(
            f"the stack at {stack} is not initialised, it holds no registry, "
            "run switchyard init to create the stack"
        )
    try:
        return os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o644)
    except OSError as exc:
        raise SwitchyardError(
            f"the lock file at {lock_path} cannot be recreated, {exc}"
        ) from exc


GIT_LOCATION_VARIABLES = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
)
GIT_CONFIG_VARIABLES = (
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
)
GIT_CONFIG_PAIR_PATTERN = re.compile(r"^GIT_CONFIG_(KEY|VALUE)_[0-9]+$")
GIT_TIMEOUT_SECONDS = 120


def _git_env():
    """The environment handed to git, with caller steering removed.

    A location variable inherited from the caller, GIT_DIR above all,
    would retarget every git call at a foreign repository even though
    this helper names its repository with -C, so the location variables
    never pass through. The config variables and the numbered config
    key and value pairs would let the caller steer what git reads as
    its configuration, so they never pass through either.
    """
    env = dict(os.environ)
    for name in GIT_LOCATION_VARIABLES + GIT_CONFIG_VARIABLES:
        env.pop(name, None)
    for name in list(env):
        if GIT_CONFIG_PAIR_PATTERN.match(name):
            env.pop(name, None)
    return env


def _git_result(repo, *args):
    try:
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            env=_git_env(),
            timeout=GIT_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as exc:
        raise SwitchyardError(
            f"git {' '.join(args)} did not finish within "
            f"{GIT_TIMEOUT_SECONDS} seconds and was stopped"
        ) from exc


def _git(repo, *args):
    result = _git_result(repo, *args)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise SwitchyardError(
            f"git {' '.join(args)} failed with exit {result.returncode}, {detail}"
        )
    return result.stdout


def _verify_own_repo(stack):
    """Refuse to run git unless the stack repo is its own repository.

    Git finds a repository by walking upward, so a stack repo that lost
    its .git entry while sitting inside a larger repository would make
    every git operation land silently in the ancestor. A stack repo
    that is a linked worktree of a foreign repository passes that test,
    because its top level is itself, while every git operation still
    lands in the foreign repository that owns the common git
    directory. Claim and release call this before they run any git
    command, and the common git directory must resolve to exactly the
    stack repo's own .git directory.
    """
    repo = stack / REPO_NAME
    if not (repo / ".git").exists():
        raise SwitchyardError(
            f"the stack repository at {repo} has no .git entry of its own, "
            "so switchyard will not run git against it"
        )
    result = _git_result(repo, "rev-parse", "--show-toplevel")
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise SwitchyardError(
            f"the stack repository at {repo} cannot be read by git, {detail}"
        )
    top = result.stdout.strip()
    if Path(top).resolve() != repo.resolve():
        raise SwitchyardError(
            f"the stack repository at {repo} resolves to the repository at "
            f"{top}, which is not the stack repository, "
            "so switchyard will not run git against it"
        )
    common = _git_result(repo, "rev-parse", "--git-common-dir")
    if common.returncode != 0:
        detail = common.stderr.strip() or common.stdout.strip()
        raise SwitchyardError(
            f"the stack repository at {repo} cannot be read by git, {detail}"
        )
    common_dir = Path(common.stdout.strip())
    if not common_dir.is_absolute():
        common_dir = repo / common_dir
    if common_dir.resolve() != (repo / ".git").resolve():
        raise SwitchyardError(
            f"the stack repository at {repo} shares the git directory at "
            f"{common_dir.resolve()}, which is not its own .git directory, "
            "so switchyard will not run git against it"
        )


def _positive_int(value, label):
    if not _is_int(value) or value <= 0:
        raise SwitchyardError(f"the {label} must be a positive integer")
    return value


def _remove_created(paths):
    problems = []
    for path in paths:
        try:
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
            elif path.exists() or path.is_symlink():
                path.unlink()
        except OSError as exc:
            problems.append(f"{path} could not be removed, {exc}")
    return problems


def init_stack(stack, port_start, range_size, range_count):
    """Create a stack root with a repository, a lock file, and a registry.

    Init is failure atomic in effect. If any step fails, everything
    init created is removed again and the failure is reported as one
    clean error, so no half initialised stack is left for later
    commands to misread.
    """
    stack = Path(stack).resolve()
    if _registry_path(stack).is_file():
        raise SwitchyardError(f"stack at {stack} is already initialised")
    port_start = _positive_int(port_start, "port start")
    range_size = _positive_int(range_size, "range size")
    range_count = _positive_int(range_count, "range count")
    if port_start + range_size * range_count - 1 > 65535:
        raise SwitchyardError("the port pool reaches past port 65535")
    repo = stack / REPO_NAME
    worktrees = stack / WORKTREES_NAME
    lock_path = stack / LOCK_NAME
    registry = _registry_path(stack)
    watched = (
        registry.with_name(registry.name + ".tmp"),
        registry,
        lock_path,
        repo,
        worktrees,
        stack,
    )
    preexisting = {path for path in watched if path.exists() or path.is_symlink()}
    try:
        worktrees.mkdir(parents=True, exist_ok=True)
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
        lock_path.touch()
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
    except BaseException as exc:
        created = [path for path in watched if path not in preexisting]
        problems = _remove_created(created)
        if problems:
            raise SwitchyardError(
                f"init of the stack at {stack} failed, {exc}, and the partial "
                f"stack could not be fully removed, {', '.join(problems)}"
            ) from exc
        if isinstance(exc, SwitchyardError):
            raise
        if isinstance(exc, OSError):
            raise SwitchyardError(
                f"the stack at {stack} cannot be created, {exc}"
            ) from exc
        raise
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


def _write_env_file(stack, env_rel, record):
    path = stack / env_rel
    try:
        path.write_text(_env_text(record), encoding="utf-8")
    except OSError as exc:
        raise SwitchyardError(
            f"the env file at {path} cannot be written, {exc}"
        ) from exc


def _require_committed_head(repo):
    result = _git_result(repo, "rev-parse", "--verify", "--quiet", "HEAD")
    if result.returncode != 0:
        raise SwitchyardError(
            f"the stack repository at {repo} has no commits yet, "
            "so no claim can branch from it"
        )


def _rollback_claim(repo, worktree, branch, branch_preexisted):
    """Best effort removal of whatever a failed claim created.

    The branch is deleted whether or not the worktree add reported
    success, because git creates the branch inside the add command
    itself, so a failed add can still leave a branch behind that would
    block every later claim for the agent. It is deleted only when
    this claim attempt created it. A branch that already stood in the
    repository before the attempt is not the claim's to remove, and it
    is left standing with whatever commits it carries. The worktree is
    removed only when it carries the .git file of a real worktree, so
    a directory that was already in the way is left untouched. Returns
    a list of problem descriptions, empty when everything was removed
    or never existed.
    """
    problems = []
    if (worktree / ".git").exists():
        result = _git_result(repo, "worktree", "remove", "--force", str(worktree))
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip()
            problems.append(
                f"the worktree at {worktree} could not be removed, {detail}"
            )
    if not branch_preexisted:
        listed = _git_result(repo, "branch", "--list", branch)
        if listed.returncode == 0 and listed.stdout.strip():
            result = _git_result(repo, "branch", "-D", branch)
            if result.returncode != 0:
                detail = result.stderr.strip() or result.stdout.strip()
                problems.append(
                    f"the branch {branch} could not be deleted, {detail}"
                )
    return problems


def claim(stack, agent):
    """Allocate a worktree, a branch, a port range, a database, an env file.

    The whole check and set runs under the stack lock, and the registry
    is loaded only after the lock is held, so a racing process can
    never act on a stale read.
    """
    stack = Path(stack).resolve()
    validate_agent_name(agent)
    repo = stack / REPO_NAME
    branch = branch_name(agent)
    worktree_rel = f"{WORKTREES_NAME}/{agent}"
    worktree = stack / worktree_rel
    env_rel = f"{worktree_rel}/{ENV_NAME}"
    with _locked(stack):
        data = load_registry(stack)
        _verify_own_repo(stack)
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
        listed = _git_result(repo, "branch", "--list", branch)
        branch_preexisted = listed.returncode == 0 and bool(listed.stdout.strip())
        try:
            _require_committed_head(repo)
            _git(repo, "worktree", "add", "-b", branch, str(worktree))
            _git(repo, "rev-parse", "--verify", f"refs/heads/{branch}")
            _write_env_file(stack, env_rel, record)
            data["claims"][agent] = record
            _save_registry(stack, data)
        except BaseException as exc:
            problems = _rollback_claim(repo, worktree, branch, branch_preexisted)
            if problems:
                raise SwitchyardError(
                    f"the claim for {agent} failed, {exc}, and rolling it "
                    f"back was incomplete, {', '.join(problems)}"
                ) from exc
            raise
        return record


def release(stack, agent):
    """Return every resource a claim holds and remove its record."""
    stack = Path(stack).resolve()
    validate_agent_name(agent)
    repo = stack / REPO_NAME
    with _locked(stack):
        data = load_registry(stack)
        _verify_own_repo(stack)
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
    stack = Path(stack).resolve()
    data = load_registry(stack)
    return [data["claims"][agent] for agent in sorted(data["claims"])]
