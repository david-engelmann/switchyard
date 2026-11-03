"""Smoke tests for the Python binding over the switchyard library.

The suite drives the full lifecycle through the binding against a
real stack the library builds on disk, worktrees and branches and
registry included, and checks the facts each call returns against
the disk and against each other. The cargo suite remains the gate
for the library itself, and this file is the consumer proof that a
Python caller can drive the same lifecycle the command line drives.
"""

import pytest

import switchyard


def test_the_full_lifecycle_through_the_binding(tmp_path):
    stack = str(tmp_path / "stack")

    pool = switchyard.init(stack)
    assert pool == {"port_start": 20000, "range_size": 10, "range_count": 16}

    alpha = switchyard.claim(stack, "alpha")
    assert alpha["agent"] == "alpha"
    assert alpha["branch"] == "switchyard/alpha"
    assert alpha["worktree"] == "worktrees/alpha"
    assert alpha["port_start"] == 20000
    assert alpha["port_end"] == 20009
    assert alpha["database"] == "switchyard_alpha"
    assert alpha["env_file"] == "worktrees/alpha/.env.switchyard"
    assert alpha["claimed_at"]

    beta = switchyard.claim(stack, "beta")
    assert beta["port_start"] == 20010
    assert beta["port_end"] == 20019

    worktree = tmp_path / "stack" / "worktrees" / "alpha"
    assert worktree.is_dir()
    assert (worktree / ".env.switchyard").is_file()

    rows = switchyard.list(stack)
    assert [row["agent"] for row in rows] == ["alpha", "beta"]
    ranges = [(row["port_start"], row["port_end"]) for row in rows]
    assert ranges == [(20000, 20009), (20010, 20019)]

    released = switchyard.release(stack, "alpha")
    assert released == alpha
    assert not worktree.exists()

    rows = switchyard.list(stack)
    assert [row["agent"] for row in rows] == ["beta"]


def test_refusals_raise_with_the_command_line_messages(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)
    switchyard.claim(stack, "alpha")

    with pytest.raises(switchyard.RefusalError) as duplicate:
        switchyard.claim(stack, "alpha")
    assert str(duplicate.value) == "agent alpha already holds a claim in this stack"

    with pytest.raises(switchyard.RefusalError) as unknown:
        switchyard.release(stack, "gamma")
    assert str(unknown.value) == "agent gamma holds no claim in this stack"

    with pytest.raises(switchyard.SwitchyardError):
        switchyard.claim(stack, "alpha")


def test_the_other_refusal_classes_raise_their_own_types(tmp_path):
    stack = str(tmp_path / "stack")

    with pytest.raises(switchyard.NotInitialisedError) as missing:
        switchyard.list(stack)
    assert "run switchyard init to create the stack" in str(missing.value)

    switchyard.init(stack)
    with pytest.raises(switchyard.InvalidNameError) as invalid:
        switchyard.claim(stack, "Not A Name")
    assert "invalid agent name" in str(invalid.value)
