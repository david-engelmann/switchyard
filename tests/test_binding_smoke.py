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


def test_init_refuses_boolean_pool_arguments(tmp_path):
    stack = str(tmp_path / "stack")

    with pytest.raises(TypeError) as start:
        switchyard.init(stack, port_start=True)
    assert "port_start" in str(start.value)

    with pytest.raises(TypeError) as size:
        switchyard.init(stack, range_size=False)
    assert "range_size" in str(size.value)

    with pytest.raises(TypeError) as count:
        switchyard.init(stack, range_count=True)
    assert "range_count" in str(count.value)

    assert not (tmp_path / "stack").exists()


def test_the_other_refusal_classes_raise_their_own_types(tmp_path):
    stack = str(tmp_path / "stack")

    with pytest.raises(switchyard.NotInitialisedError) as missing:
        switchyard.list(stack)
    assert "run switchyard init to create the stack" in str(missing.value)

    switchyard.init(stack)
    with pytest.raises(switchyard.InvalidNameError) as invalid:
        switchyard.claim(stack, "Not A Name")
    assert "invalid agent name" in str(invalid.value)


def _verdict_body(choice, confidence):
    import json

    rest = (1.0 - confidence) / 3.0
    probabilities = {
        label: (confidence if label == choice else rest)
        for label in ("lean", "standard", "roomy", "other")
    }
    return json.dumps(
        {
            "model": "jev-1.13.0",
            "answers": {
                "profile": {
                    "type": "choice",
                    "choice": choice,
                    "confidence": 0.5,
                    "probabilities": probabilities,
                }
            },
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
    )


def test_a_profiled_claim_through_the_binding(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    record = switchyard.claim_profile(stack, "alpha", "roomy")
    assert (record["port_start"], record["port_end"]) == (20000, 20039)

    with pytest.raises(switchyard.RefusalError) as unknown:
        switchyard.claim_profile(stack, "beta", "huge")
    assert "unknown profile huge" in str(unknown.value)


def test_a_routed_claim_through_a_callable_transport(tmp_path):
    import json

    stack = str(tmp_path / "stack")
    switchyard.init(stack)
    seen = []

    def judge(request):
        seen.append(request)
        return _verdict_body("standard", 0.9)

    record = switchyard.claim_routed(
        stack, "alpha", "Run the app and its API and a worker.", "lean", judge
    )
    assert (record["port_start"], record["port_end"]) == (20000, 20019)
    assert len(seen) == 1
    assert '"jev-1.13.0"' in seen[0]
    assert '"profile"' in seen[0]

    lines = (tmp_path / "stack" / "switchyard-routing.jsonl").read_text().splitlines()
    assert len(lines) == 1
    line = json.loads(lines[0])
    assert line["fallback"] == "none"
    assert line["verdict"] == "standard"
    assert line["allocated"] == "standard"
    assert line["confidence"] == 0.9


def test_a_routed_claim_with_no_transport_and_with_a_raising_callable(tmp_path):
    import json

    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    record = switchyard.claim_routed(stack, "alpha", "Run one server.", "standard")
    assert (record["port_start"], record["port_end"]) == (20000, 20019)

    def broken(request):
        raise RuntimeError("the judge is down in this test")

    record = switchyard.claim_routed(stack, "beta", "Run one server.", "lean", broken)
    assert (record["port_start"], record["port_end"]) == (20020, 20029)

    lines = (tmp_path / "stack" / "switchyard-routing.jsonl").read_text().splitlines()
    assert len(lines) == 2
    first = json.loads(lines[0])
    second = json.loads(lines[1])
    assert first["fallback"] == "no transport"
    assert first["model"] is None
    assert second["fallback"] == "transport"
    assert second["allocated"] == "lean"
