"""Tests for the switchyard slice 1 obligations SY-1 to SY-10.

Every fixture is synthetic. Stacks are built in pytest temporary
directories, the git repositories inside them are created by the init
command under test, and the concurrency tests race real operating
system processes against one registry behind a shared start barrier.
"""

import io
import json
import multiprocessing
import os
import shutil
import subprocess
import sys
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]

from switchyard import cli, core  # noqa: E402


def run_cli(argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        try:
            code = cli.main(argv)
        except SystemExit as exc:
            code = exc.code if isinstance(exc.code, int) else 2
    return code, out.getvalue(), err.getvalue()


def init_stack_dir(base, **pool):
    stack = Path(base) / "stack"
    argv = ["init", "--stack", str(stack)]
    for key, value in pool.items():
        argv += [f"--{key.replace('_', '-')}", str(value)]
    code, _, err = run_cli(argv)
    assert code == 0, err
    return stack


@pytest.fixture()
def stack(tmp_path):
    return init_stack_dir(tmp_path)


def read_registry(stack):
    return json.loads((stack / core.REGISTRY_NAME).read_text(encoding="utf-8"))


def git(repo, *args):
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )
    assert result.returncode == 0, result.stderr
    return result.stdout


def snapshot(root):
    result = {}
    for path in sorted(Path(root).rglob("*")):
        rel = str(path.relative_to(root))
        result[rel] = path.read_bytes() if path.is_file() else None
    return result


# SY-1, complete allocation


def test_init_records_the_pool_and_creates_the_repository(stack):
    data = read_registry(stack)
    assert data["version"] == 1
    assert data["port_pool"] == {"start": 20000, "range_size": 10, "range_count": 16}
    assert data["claims"] == {}
    head = git(stack / "repo", "rev-parse", "--abbrev-ref", "HEAD")
    assert head.strip() == "main"


def test_claim_allocates_everything_and_the_record_matches_the_disk(stack):
    code, out, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code == 0, err
    data = read_registry(stack)
    assert list(data["claims"]) == ["alpha"]
    record = data["claims"]["alpha"]
    assert record["branch"] == "switchyard/alpha"
    assert record["database"] == "switchyard_alpha"
    assert record["port_start"] == 20000
    assert record["port_end"] == 20009
    pool = data["port_pool"]
    assert pool["start"] <= record["port_start"] <= record["port_end"]
    assert record["port_end"] <= pool["start"] + pool["range_size"] * pool["range_count"] - 1
    worktree = stack / record["worktree"]
    assert worktree.is_dir()
    head = git(worktree, "rev-parse", "--abbrev-ref", "HEAD")
    assert head.strip() == "switchyard/alpha"
    branches = git(stack / "repo", "branch", "--list", "switchyard/alpha")
    assert "switchyard/alpha" in branches
    env_file = stack / record["env_file"]
    assert env_file.is_file()
    assert env_file.parent == worktree
    assert "switchyard/alpha" in out
    assert "20000-20009" in out


def test_env_file_contents_match_the_registry_record(stack):
    code, _, err = run_cli(["claim", "beta-one", "--stack", str(stack)])
    assert code == 0, err
    record = read_registry(stack)["claims"]["beta-one"]
    env = {}
    for line in (stack / record["env_file"]).read_text(encoding="utf-8").splitlines():
        if "=" in line and not line.startswith("#"):
            key, value = line.split("=", 1)
            env[key] = value
    assert env["SWITCHYARD_AGENT"] == "beta-one"
    assert env["SWITCHYARD_BRANCH"] == record["branch"]
    assert env["SWITCHYARD_DATABASE"] == record["database"] == "switchyard_beta_one"
    assert int(env["SWITCHYARD_PORT_START"]) == record["port_start"]
    assert int(env["SWITCHYARD_PORT_END"]) == record["port_end"]
    assert env["DATABASE_URL"].endswith("/" + record["database"])
    assert f":{record['port_start']}/" in env["DATABASE_URL"]


# SY-2, one claim per agent


def test_second_claim_for_the_same_agent_fails_and_changes_nothing(stack):
    code, _, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code == 0, err
    registry_before = (stack / core.REGISTRY_NAME).read_bytes()
    worktrees_before = sorted(p.name for p in (stack / "worktrees").iterdir())
    code, _, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code != 0
    assert "alpha" in err
    assert (stack / core.REGISTRY_NAME).read_bytes() == registry_before
    assert sorted(p.name for p in (stack / "worktrees").iterdir()) == worktrees_before


# SY-3, the port pool


def test_ranges_never_overlap_and_the_pool_can_run_out(tmp_path):
    small = init_stack_dir(tmp_path, range_count=2)
    assert run_cli(["claim", "one", "--stack", str(small)])[0] == 0
    assert run_cli(["claim", "two", "--stack", str(small)])[0] == 0
    claims = read_registry(small)["claims"]
    ranges = sorted(
        (record["port_start"], record["port_end"]) for record in claims.values()
    )
    assert ranges == [(20000, 20009), (20010, 20019)]
    code, _, err = run_cli(["claim", "three", "--stack", str(small)])
    assert code != 0
    assert "exhausted" in err
    assert set(read_registry(small)["claims"]) == {"one", "two"}
    assert not (small / "worktrees" / "three").exists()


# SY-4, atomicity under concurrency


def _race_claim(stack_path, agent, barrier, queue):
    out, err = io.StringIO(), io.StringIO()
    try:
        barrier.wait(timeout=60)
        with redirect_stdout(out), redirect_stderr(err):
            code = cli.main(["claim", agent, "--stack", str(stack_path)])
        queue.put(("done", agent, code))
    except Exception as exc:
        queue.put(("crash", agent, repr(exc)))


def _race(stack, agents):
    context = multiprocessing.get_context("fork")
    barrier = context.Barrier(len(agents))
    queue = context.Queue()
    processes = [
        context.Process(target=_race_claim, args=(str(stack), agent, barrier, queue))
        for agent in agents
    ]
    for process in processes:
        process.start()
    results = [queue.get(timeout=120) for _ in processes]
    for process in processes:
        process.join(timeout=30)
        if process.is_alive():
            process.terminate()
    assert all(result[0] == "done" for result in results), results
    return results


def test_racing_claims_for_one_agent_produce_exactly_one_success(stack):
    results = _race(stack, ["alpha"] * 6)
    codes = sorted(result[2] for result in results)
    assert codes == [0, 1, 1, 1, 1, 1]
    data = read_registry(stack)
    assert list(data["claims"]) == ["alpha"]
    worktrees = [p for p in (stack / "worktrees").iterdir() if p.is_dir()]
    assert [p.name for p in worktrees] == ["alpha"]


def test_racing_claims_for_distinct_agents_never_overlap(tmp_path):
    small = init_stack_dir(tmp_path, range_count=3)
    agents = ["racer-a", "racer-b", "racer-c", "racer-d", "racer-e"]
    results = _race(small, agents)
    codes = [result[2] for result in results]
    assert codes.count(0) == 3
    assert codes.count(1) == 2
    claims = read_registry(small)["claims"]
    assert len(claims) == 3
    ranges = sorted(
        (record["port_start"], record["port_end"]) for record in claims.values()
    )
    for (_, prev_end), (start, _) in zip(ranges, ranges[1:]):
        assert start > prev_end
    winners = {result[1] for result in results if result[2] == 0}
    assert set(claims) == winners
    worktrees = {p.name for p in (small / "worktrees").iterdir() if p.is_dir()}
    assert worktrees == winners


# SY-5, the list view


def test_list_prints_exactly_the_registry_claims(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    assert run_cli(["claim", "beta", "--stack", str(stack)])[0] == 0
    (stack / "worktrees" / "ghost").mkdir()
    (stack / "stray.txt").write_text("not a claim", encoding="utf-8")
    code, out, err = run_cli(["list", "--stack", str(stack)])
    assert code == 0, err
    assert "alpha" in out
    assert "beta" in out
    assert "switchyard_alpha" in out
    assert "20010-20019" in out
    assert "ghost" not in out
    data_rows = [
        line for line in out.splitlines() if line.strip() and not line.startswith("AGENT")
    ]
    assert len(data_rows) == 2
    assert run_cli(["release", "beta", "--stack", str(stack)])[0] == 0
    code, out, _ = run_cli(["list", "--stack", str(stack)])
    assert code == 0
    assert "beta" not in out
    assert "alpha" in out


# SY-6, complete release


def test_release_returns_every_resource_and_frees_the_range(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    record = read_registry(stack)["claims"]["alpha"]
    worktree = stack / record["worktree"]
    env_file = stack / record["env_file"]
    (worktree / "scratch.txt").write_text("agent scratch", encoding="utf-8")
    code, _, err = run_cli(["release", "alpha", "--stack", str(stack)])
    assert code == 0, err
    assert not worktree.exists()
    assert not env_file.exists()
    assert git(stack / "repo", "branch", "--list", "switchyard/alpha").strip() == ""
    assert read_registry(stack)["claims"] == {}
    code, _, err = run_cli(["claim", "beta", "--stack", str(stack)])
    assert code == 0, err
    reused = read_registry(stack)["claims"]["beta"]
    assert (reused["port_start"], reused["port_end"]) == (
        record["port_start"],
        record["port_end"],
    )


# SY-7, unknown release


def test_release_of_an_unknown_agent_is_an_error_and_changes_nothing(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    registry_before = (stack / core.REGISTRY_NAME).read_bytes()
    code, _, err = run_cli(["release", "nobody", "--stack", str(stack)])
    assert code != 0
    assert "nobody" in err
    assert (stack / core.REGISTRY_NAME).read_bytes() == registry_before
    assert (stack / "worktrees" / "alpha").is_dir()


# SY-8, a corrupt registry


CORRUPT_BODIES = [
    "this is not json {",
    json.dumps({"version": 99, "port_pool": {}, "claims": {}}),
    json.dumps(
        {
            "version": 1,
            "port_pool": {"start": 20000, "range_size": 10, "range_count": 4},
            "claims": ["alpha"],
        }
    ),
    json.dumps(
        {
            "version": 1,
            "port_pool": {"start": 20000, "range_size": 10, "range_count": 4},
            "claims": {
                "alpha": {
                    "agent": "alpha",
                    "branch": "switchyard/alpha",
                    "worktree": "worktrees/alpha",
                    "port_start": 20000,
                    "port_end": 20009,
                    "database": "switchyard_alpha",
                    "env_file": "worktrees/alpha/.env.switchyard",
                    "claimed_at": "2026-11-02T00:00:00+00:00",
                },
                "beta": {
                    "agent": "beta",
                    "branch": "switchyard/beta",
                    "worktree": "worktrees/beta",
                    "port_start": 20005,
                    "port_end": 20014,
                    "database": "switchyard_beta",
                    "env_file": "worktrees/beta/.env.switchyard",
                    "claimed_at": "2026-11-02T00:00:00+00:00",
                },
            },
        }
    ),
]


def valid_record(agent, port_start=20000):
    return {
        "agent": agent,
        "branch": core.branch_name(agent),
        "worktree": f"{core.WORKTREES_NAME}/{agent}",
        "port_start": port_start,
        "port_end": port_start + 9,
        "database": core.database_name(agent),
        "env_file": f"{core.WORKTREES_NAME}/{agent}/{core.ENV_NAME}",
        "claimed_at": "2026-11-02T00:00:00+00:00",
    }


def registry_body(claims, **overrides):
    body = {
        "version": 1,
        "port_pool": {"start": 20000, "range_size": 10, "range_count": 16},
        "claims": claims,
    }
    body.update(overrides)
    return json.dumps(body)


def _forged_bodies():
    extra_record_field = valid_record("alpha")
    extra_record_field["note"] = "not a recorded field"
    missing_claimed_at = valid_record("alpha")
    del missing_claimed_at["claimed_at"]
    forged_branch = valid_record("alpha")
    forged_branch["branch"] = "main"
    forged_worktree = valid_record("alpha")
    forged_worktree["worktree"] = "worktrees/beta"
    forged_worktree["env_file"] = "worktrees/beta/" + core.ENV_NAME
    wide_pool = {"start": 20000, "range_size": 10, "range_count": 16, "extra": 1}
    return [
        registry_body({}, version=1.0),
        registry_body({}, version=True),
        registry_body({}, surprise="an unrecorded field"),
        registry_body({}, port_pool=wide_pool),
        registry_body({"alpha": extra_record_field}),
        registry_body({"alpha": missing_claimed_at}),
        registry_body({"alpha": forged_branch}),
        registry_body({"alpha": forged_worktree}),
        registry_body({"../escape": valid_record("../escape")}),
    ]


CORRUPT_BODIES += _forged_bodies()


@pytest.mark.parametrize("body", CORRUPT_BODIES)
def test_a_corrupt_registry_fails_every_command_and_changes_nothing(stack, body):
    (stack / core.REGISTRY_NAME).write_text(body, encoding="utf-8")
    before = snapshot(stack)
    for argv in (
        ["claim", "alpha", "--stack", str(stack)],
        ["list", "--stack", str(stack)],
        ["release", "alpha", "--stack", str(stack)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert "corrupt" in err, argv
    assert snapshot(stack) == before


# SY-9, an uninitialised stack


def test_an_uninitialised_stack_fails_every_command_and_creates_nothing(tmp_path):
    empty = tmp_path / "empty"
    empty.mkdir()
    missing = tmp_path / "missing"
    for target in (empty, missing):
        for argv in (
            ["claim", "alpha", "--stack", str(target)],
            ["list", "--stack", str(target)],
            ["release", "alpha", "--stack", str(target)],
        ):
            code, _, err = run_cli(argv)
            assert code != 0, argv
            assert err.strip(), argv
    assert list(empty.iterdir()) == []
    assert not missing.exists()


# SY-10, agent name safety


@pytest.mark.parametrize(
    "name", ["../escape", "a/b", "two words", "Upper", ".hidden", "-lead", ""]
)
def test_unsafe_agent_names_are_rejected_and_change_nothing(stack, name):
    before = snapshot(stack)
    code, _, err = run_cli(["claim", name, "--stack", str(stack)])
    assert code != 0
    assert err.strip()
    assert snapshot(stack) == before
    assert not (stack.parent / "escape").exists()


# SY-8, forged records, decode damage, and the repository perimeter


def test_a_record_pointing_at_another_agents_worktree_is_corrupt(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    assert run_cli(["claim", "beta", "--stack", str(stack)])[0] == 0
    data = read_registry(stack)
    data["claims"]["alpha"]["worktree"] = "worktrees/beta"
    data["claims"]["alpha"]["env_file"] = "worktrees/beta/" + core.ENV_NAME
    (stack / core.REGISTRY_NAME).write_text(json.dumps(data), encoding="utf-8")
    before = snapshot(stack)
    code, _, err = run_cli(["release", "alpha", "--stack", str(stack)])
    assert code != 0
    assert "corrupt" in err
    assert snapshot(stack) == before
    assert (stack / "worktrees" / "beta").is_dir()


@pytest.mark.parametrize(
    "blob",
    [
        b"\xff\xfe\x00 not a registry in any encoding",
        json.dumps({"version": 1, "port_pool": {}, "claims": {}}).encode("utf-16"),
    ],
)
def test_a_registry_that_is_not_utf8_text_is_corrupt_and_changes_nothing(stack, blob):
    (stack / core.REGISTRY_NAME).write_bytes(blob)
    before = snapshot(stack)
    for argv in (
        ["claim", "alpha", "--stack", str(stack)],
        ["list", "--stack", str(stack)],
        ["release", "alpha", "--stack", str(stack)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert "corrupt" in err, argv
    assert snapshot(stack) == before


def test_a_registry_path_that_is_a_directory_is_corrupt_not_missing(stack):
    registry = stack / core.REGISTRY_NAME
    registry.unlink()
    registry.mkdir()
    for argv in (
        ["claim", "alpha", "--stack", str(stack)],
        ["list", "--stack", str(stack)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert "corrupt" in err, argv
        assert "not a file" in err, argv


def test_a_lock_path_that_is_a_directory_is_a_clean_error(stack):
    lock = stack / core.LOCK_NAME
    lock.unlink()
    lock.mkdir()
    for argv in (
        ["claim", "alpha", "--stack", str(stack)],
        ["release", "alpha", "--stack", str(stack)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert "lock" in err, argv


def test_a_missing_lock_file_is_recreated_when_the_registry_survives(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    (stack / core.LOCK_NAME).unlink()
    code, _, err = run_cli(["claim", "beta", "--stack", str(stack)])
    assert code == 0, err
    assert (stack / core.LOCK_NAME).is_file()
    code, _, err = run_cli(["release", "alpha", "--stack", str(stack)])
    assert code == 0, err
    assert set(read_registry(stack)["claims"]) == {"beta"}


def test_claim_refuses_a_stack_repo_that_lost_its_git_inside_an_outer_repo(tmp_path):
    git(tmp_path, "init", "-q", "-b", "main", "outer")
    outer = tmp_path / "outer"
    stack = init_stack_dir(outer)
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    registry_before = (stack / core.REGISTRY_NAME).read_bytes()
    shutil.rmtree(stack / "repo" / ".git")
    for argv in (
        ["claim", "beta", "--stack", str(stack)],
        ["release", "alpha", "--stack", str(stack)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert ".git" in err, argv
    assert (stack / core.REGISTRY_NAME).read_bytes() == registry_before
    assert "switchyard" not in git(outer, "branch", "--list")
    assert str(stack) not in git(outer, "worktree", "list")


# SY-1 and SY-9, claim failure completeness and stack path handling


def test_a_blocked_claim_leaves_no_branch_and_the_name_can_be_claimed(stack):
    blocker = stack / "worktrees" / "alpha"
    blocker.mkdir()
    (blocker / "junk.txt").write_text("in the way", encoding="utf-8")
    code, _, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code != 0
    assert git(stack / "repo", "branch", "--list", "switchyard/alpha").strip() == ""
    assert read_registry(stack)["claims"] == {}
    shutil.rmtree(blocker)
    code, _, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code == 0, err
    assert (stack / "worktrees" / "alpha").is_dir()


def test_a_failed_claim_reports_an_incomplete_rollback(stack, monkeypatch):
    real_result = core._git_result

    def flaky_result(repo, *args):
        if args[:2] == ("branch", "-D"):
            return subprocess.CompletedProcess(args, 1, "", "the branch is busy")
        return real_result(repo, *args)

    def broken_save(stack_arg, data):
        raise core.SwitchyardError("the registry write is broken in this test")

    monkeypatch.setattr(core, "_git_result", flaky_result)
    monkeypatch.setattr(core, "_save_registry", broken_save)
    with pytest.raises(core.SwitchyardError) as excinfo:
        core.claim(stack, "alpha")
    message = str(excinfo.value)
    assert "rolling it back was incomplete" in message
    assert "could not be deleted" in message


def test_a_failed_registry_save_leaves_no_temporary_file(stack, monkeypatch):
    def broken_replace(source, target):
        raise OSError("no space left in this test")

    monkeypatch.setattr(core.os, "replace", broken_replace)
    with pytest.raises(core.SwitchyardError):
        core._save_registry(stack, read_registry(stack))
    assert not (stack / (core.REGISTRY_NAME + ".tmp")).exists()
    assert (stack / core.REGISTRY_NAME).is_file()


def test_claim_against_a_repository_with_no_commits_fails_and_records_nothing(stack):
    shutil.rmtree(stack / "repo")
    git(stack, "init", "-q", "-b", "main", "repo")
    code, _, err = run_cli(["claim", "alpha", "--stack", str(stack)])
    assert code != 0
    assert "no commits" in err
    assert read_registry(stack)["claims"] == {}
    assert not (stack / "worktrees" / "alpha").exists()


def test_a_relative_stack_path_works_end_to_end(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    code, _, err = run_cli(["init", "--stack", "stack"])
    assert code == 0, err
    code, _, err = run_cli(["claim", "alpha", "--stack", "stack"])
    assert code == 0, err
    stack = tmp_path / "stack"
    assert (stack / "worktrees" / "alpha").is_dir()
    assert not (stack / "repo" / "stack").exists()
    code, out, err = run_cli(["list", "--stack", "stack"])
    assert code == 0, err
    assert "alpha" in out
    code, _, err = run_cli(["release", "alpha", "--stack", "stack"])
    assert code == 0, err
    assert read_registry(stack)["claims"] == {}


def test_a_stack_root_that_is_a_file_fails_every_command_cleanly(tmp_path):
    target = tmp_path / "stack"
    target.write_text("a file, not a stack", encoding="utf-8")
    for argv in (
        ["init", "--stack", str(target)],
        ["claim", "alpha", "--stack", str(target)],
        ["list", "--stack", str(target)],
        ["release", "alpha", "--stack", str(target)],
    ):
        code, _, err = run_cli(argv)
        assert code != 0, argv
        assert err.strip(), argv
    assert target.read_text(encoding="utf-8") == "a file, not a stack"


def test_a_failed_init_leaves_no_half_initialised_stack(tmp_path):
    stack = tmp_path / "stack"
    stack.mkdir()
    (stack / "repo").write_text("a file where the repo belongs", encoding="utf-8")
    code, _, err = run_cli(["init", "--stack", str(stack)])
    assert code != 0
    assert err.strip()
    assert sorted(p.name for p in stack.iterdir()) == ["repo"]
    assert (stack / "repo").read_text(encoding="utf-8") == (
        "a file where the repo belongs"
    )
    code, _, err = run_cli(["list", "--stack", str(stack)])
    assert code != 0
    assert "no registry" in err


# SY-10, the name length cap


def test_the_agent_name_cap_is_forty_characters(stack):
    code, _, err = run_cli(["claim", "a" * 41, "--stack", str(stack)])
    assert code != 0
    assert "40" in err
    assert read_registry(stack)["claims"] == {}
    code, _, err = run_cli(["claim", "a" * 40, "--stack", str(stack)])
    assert code == 0, err


# The module entry point


def test_the_module_entry_point_lists_a_live_claim(stack):
    assert run_cli(["claim", "alpha", "--stack", str(stack)])[0] == 0
    env = dict(os.environ)
    env["PYTHONPATH"] = str(ROOT / "src")
    result = subprocess.run(
        [sys.executable, "-m", "switchyard", "list", "--stack", str(stack)],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, result.stderr
    assert "alpha" in result.stdout
    assert "switchyard_alpha" in result.stdout
