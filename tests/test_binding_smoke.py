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

def test_init_pool_arguments_stand_keyword_only(tmp_path):
    stack = str(tmp_path / "stack")

    # The pool arguments stand keyword-only, as the optional
    # arguments of the claim functions do, so a caller cannot
    # transpose range_size and range_count by position and build
    # a pool of the wrong shape in silence. This pin fails on
    # the old signature for the real reason that it accepted
    # the positional call and built the stack, and its keyword
    # half names non-default values for all three arguments,
    # so a build that ignored a keyword value would fail it.
    with pytest.raises(TypeError):
        switchyard.init(stack, 20000, 10, 16)
    assert not (tmp_path / "stack").exists()

    with pytest.raises(TypeError):
        switchyard.init(stack, 20000)
    assert not (tmp_path / "stack").exists()

    pool = switchyard.init(
        stack, port_start=21000, range_size=5, range_count=4
    )
    assert pool == {"port_start": 21000, "range_size": 5, "range_count": 4}


def test_the_other_refusal_classes_raise_their_own_types(tmp_path):
    stack = str(tmp_path / "stack")

    with pytest.raises(switchyard.NotInitialisedError) as missing:
        switchyard.list(stack)
    assert "run switchyard init to create the stack" in str(missing.value)

    switchyard.init(stack)
    with pytest.raises(switchyard.InvalidNameError) as invalid:
        switchyard.claim(stack, "Not A Name")
    assert "invalid agent name" in str(invalid.value)


def test_a_corrupt_registry_raises_the_corrupt_class_through_the_binding(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    registry = tmp_path / "stack" / "switchyard-registry.json"
    registry.write_text("this is not json")

    # The binding maps each refusal class to its own exception
    # subclass, and the corrupt registry class is the one the
    # Rust suites can only ever meet as exit code 2, so this pin
    # is its only consumer proof. It fails if the mapping ever
    # classes a corrupt registry as a plain refusal or as the
    # base error, because the raises check names the subclass.
    with pytest.raises(switchyard.CorruptRegistryError) as corrupt:
        switchyard.list(stack)
    assert "corrupt" in str(corrupt.value)


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
        stack, "alpha", "Run the app and its API and a worker.", "lean", transport=judge
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

    record = switchyard.claim_routed(stack, "beta", "Run one server.", "lean", transport=broken)
    assert (record["port_start"], record["port_end"]) == (20020, 20029)

    lines = (tmp_path / "stack" / "switchyard-routing.jsonl").read_text().splitlines()
    assert len(lines) == 2
    first = json.loads(lines[0])
    second = json.loads(lines[1])
    assert first["fallback"] == "no transport"
    assert first["model"] is None
    assert second["fallback"] == "transport"
    assert second["allocated"] == "lean"


def _score_body(choice, confidence):
    import json

    labels = [str(level) for level in range(1, 11)] + ["other"]
    rest = (1.0 - confidence) / 10.0
    probabilities = {
        label: (confidence if label == choice else rest) for label in labels
    }
    return json.dumps(
        {
            "model": "jev-1.13.0",
            "answers": {
                "score": {
                    "type": "choice",
                    "choice": choice,
                    "confidence": 0.5,
                    "probabilities": probabilities,
                }
            },
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
    )


def test_a_waiting_claim_through_the_binding(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack, range_count=1)
    switchyard.claim(stack, "holder")

    wait = switchyard.claim(stack, "waiter", wait=True, wait_for=120)
    assert wait["agent"] == "waiter"
    assert wait["profile"] == "plain"
    assert wait["score"] is None
    assert wait["confidence"] is None
    assert wait["model"] is None
    assert wait["fallback"] == "no transport"
    assert wait["wait_for"] == 120
    assert wait["recorded_at"]

    rows = switchyard.queue(stack)
    assert [row["agent"] for row in rows] == ["waiter"]
    assert rows[0]["profile"] == "plain"

    released = switchyard.release(stack, "waiter")
    assert released == wait
    assert switchyard.queue(stack) == []

    offered = switchyard.release(stack, "holder")
    assert offered["agent"] == "holder"


def test_a_scored_wait_through_a_callable_transport(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack, range_count=1)
    switchyard.claim(stack, "holder")
    seen = []

    def judge(request):
        seen.append(request)
        return _score_body("8", 0.75)

    wait = switchyard.claim(stack, "waiter", wait=True, transport=judge)
    assert wait["score"] == 8
    assert wait["confidence"] == 0.75
    assert wait["fallback"] == "none"
    assert wait["model"] == "jev-1.13.0"
    assert len(seen) == 1
    assert '"score"' in seen[0]

    released = switchyard.release(stack, "holder")
    assert released["agent"] == "holder"
    rows = switchyard.list(stack)
    assert [row["agent"] for row in rows] == ["waiter"]
    assert (rows[0]["port_start"], rows[0]["port_end"]) == (20000, 20009)
    assert switchyard.queue(stack) == []


def test_a_routed_waiting_claim_through_a_callable_transport(tmp_path):
    import json

    stack = str(tmp_path / "stack")
    switchyard.init(stack, range_count=2)
    switchyard.claim(stack, "holder")
    seen = []

    def judge(request):
        seen.append(request)
        if '"profile"' in request:
            return _verdict_body("standard", 0.9)
        return _score_body("5", 0.6)

    wait = switchyard.claim_routed(
        stack,
        "waiter",
        "Run the app and its API and a worker.",
        "lean",
        transport=judge,
        wait=True,
    )
    assert wait["profile"] == "standard"
    assert wait["score"] == 5
    assert wait["confidence"] == 0.6
    assert len(seen) == 2

    lines = (tmp_path / "stack" / "switchyard-routing.jsonl").read_text().splitlines()
    assert len(lines) == 1
    line = json.loads(lines[0])
    assert line["refusal"] == "queued"
    assert line["verdict"] == "standard"
    assert line["allocated"] is None


def test_a_transport_beside_a_claim_that_does_not_wait_is_refused(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    def judge(request):
        return "{}"

    with pytest.raises(switchyard.RefusalError) as caught:
        switchyard.claim(stack, "alpha", transport=judge)
    assert "transport" in str(caught.value)

    with pytest.raises(switchyard.RefusalError):
        switchyard.claim_profile(stack, "alpha", "lean", transport=judge)

    assert switchyard.list(stack) == []


def test_the_wait_bound_refusals_of_the_binding(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    with pytest.raises(switchyard.RefusalError) as beside:
        switchyard.claim(stack, "waiter", wait_for=30)
    assert "wait" in str(beside.value)

    with pytest.raises(TypeError) as boolean:
        switchyard.claim(stack, "waiter", wait=True, wait_for=True)
    assert "wait_for" in str(boolean.value)

    with pytest.raises(switchyard.RefusalError) as bound:
        switchyard.claim(stack, "waiter", wait=True, wait_for=0)
    assert "positive" in str(bound.value)


def test_an_oversized_pool_argument_is_a_named_refusal(tmp_path):
    stack = str(tmp_path / "stack")

    with pytest.raises(switchyard.RefusalError) as caught:
        switchyard.init(stack, port_start=10**40)
    assert "port_start" in str(caught.value)
    assert "128-bit" in str(caught.value)
    assert not (tmp_path / "stack").exists()


def test_an_oversized_wait_bound_is_a_named_refusal(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    with pytest.raises(switchyard.RefusalError) as caught:
        switchyard.claim(stack, "waiter", wait=True, wait_for=10**40)
    assert "wait_for" in str(caught.value)
    assert "64-bit" in str(caught.value)
    assert switchyard.list(stack) == []


def test_the_claim_functions_take_their_options_by_keyword_only(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    def judge(request):
        return "{}"

    with pytest.raises(TypeError):
        switchyard.claim(stack, "alpha", False, None, judge)
    with pytest.raises(TypeError):
        switchyard.claim_profile(stack, "alpha", "lean", False, None, judge)
    with pytest.raises(TypeError):
        switchyard.claim_routed(stack, "alpha", "Run one server.", "lean", judge)

    assert switchyard.list(stack) == []


def test_a_pool_argument_of_a_wrong_type_raises_type_error(tmp_path):
    stack = str(tmp_path / "stack")

    # A value of a wrong type at extraction passes through as
    # the TypeError the extraction raises, beside the one width
    # failure the binding maps to a named RefusalError and the
    # boolean shape it refuses by name. A build that mapped
    # every extraction failure to RefusalError would fail this
    # pin, because the documented TypeError would arrive as a
    # SwitchyardError instead.
    with pytest.raises(TypeError) as caught:
        switchyard.init(stack, port_start="20000")
    assert not isinstance(caught.value, switchyard.SwitchyardError)
    assert not (tmp_path / "stack").exists()


def test_a_wait_bound_of_a_wrong_type_raises_type_error(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    with pytest.raises(TypeError) as caught:
        switchyard.claim(stack, "waiter", wait=True, wait_for="30")
    assert not isinstance(caught.value, switchyard.SwitchyardError)
    assert switchyard.list(stack) == []


def test_a_transport_returning_a_non_text_value_lands_as_transport(tmp_path):
    import json

    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    # A callable that returns a value that is not text surfaces
    # as a transport failure, so the claim lands on its declared
    # default under the transport class. A build that stringified
    # the return would land the claim under the malformed class,
    # and a build that let the extraction failure escape as an
    # exception would break the claim instead of landing it, and
    # this pin fails against either. The callable is passed by
    # keyword, so the pin stands true on both sides of the claim
    # family's keyword only signatures.
    def judge(request):
        return {"not": "text"}

    record = switchyard.claim_routed(
        stack, "alpha", "Run one server.", "lean", transport=judge
    )
    assert (record["port_start"], record["port_end"]) == (20000, 20009)

    lines = (tmp_path / "stack" / "switchyard-routing.jsonl").read_text().splitlines()
    assert len(lines) == 1
    line = json.loads(lines[0])
    assert line["fallback"] == "transport"
    assert line["verdict"] is None
    assert line["allocated"] == "lean"

def test_a_waiting_profiled_claim_through_the_binding(tmp_path):
    stack = str(tmp_path / "stack")
    switchyard.init(stack, range_count=2)
    switchyard.claim(stack, "holder")
    wait = switchyard.claim_profile(stack, "waiter", "standard", wait=True, wait_for=120)
    assert wait["agent"] == "waiter"
    assert wait["profile"] == "standard"
    assert wait["wait_for"] == 120
    assert wait["score"] is None
    assert wait["confidence"] is None
    assert wait["model"] is None
    assert wait["fallback"] == "no transport"
    assert wait["recorded_at"]
    queue = switchyard.queue(stack)
    assert [entry["agent"] for entry in queue] == ["waiter"]
    assert queue[0]["profile"] == "standard"

    def judge(description):
        return _score_body("7", 0.8)

    scored = switchyard.claim_profile(
        stack, "scored", "roomy", wait=True, transport=judge
    )
    assert scored["profile"] == "roomy"
    assert scored["score"] == 7
    assert scored["confidence"] == 0.8
    assert scored["fallback"] == "none"
    assert scored["model"] == "jev-1.13.0"
    switchyard.release(stack, "holder")
    claims = switchyard.list(stack)
    assert [claim["agent"] for claim in claims] == ["waiter"]
    assert (claims[0]["port_start"], claims[0]["port_end"]) == (20000, 20019)
    queue = switchyard.queue(stack)
    assert [entry["agent"] for entry in queue] == ["scored"]


def test_init_from_repo_and_branch_through_the_binding(tmp_path):
    import json
    import subprocess

    src = tmp_path / "src_repo"
    src.mkdir()
    subprocess.run(["git", "-C", str(src), "init", "-q", "-b", "trunk"], check=True)
    subprocess.run(
        ["git", "-C", str(src), "config", "user.email", "py@example.invalid"],
        check=True,
    )
    subprocess.run(
        ["git", "-C", str(src), "config", "user.name", "py"],
        check=True,
    )
    (src / "README.md").write_text("trunk\n")
    subprocess.run(["git", "-C", str(src), "add", "README.md"], check=True)
    subprocess.run(["git", "-C", str(src), "commit", "-q", "-m", "trunk"], check=True)
    subprocess.run(
        ["git", "-C", str(src), "checkout", "-q", "-b", "feature-base"], check=True
    )
    (src / "feature.txt").write_text("feature\n")
    subprocess.run(["git", "-C", str(src), "add", "feature.txt"], check=True)
    subprocess.run(["git", "-C", str(src), "commit", "-q", "-m", "feature"], check=True)
    subprocess.run(["git", "-C", str(src), "checkout", "-q", "trunk"], check=True)

    with pytest.raises(switchyard.RefusalError):
        switchyard.init(
            str(tmp_path / "bad_url"), from_repo="https://example.invalid/repo.git"
        )

    stack = str(tmp_path / "stack")
    pool = switchyard.init(stack, from_repo=src, branch="feature-base")
    assert pool == {"port_start": 20000, "range_size": 10, "range_count": 16}

    cfg = json.loads((tmp_path / "stack" / "switchyard-config.json").read_text())
    assert cfg["base_branch"] == "feature-base"
    assert cfg["version"] == 1

    alpha = switchyard.claim(stack, "alpha")
    assert alpha["branch"] == "switchyard/alpha"
    assert (tmp_path / "stack" / "worktrees" / "alpha" / "feature.txt").is_file()


def test_safe_release_keep_branch_and_doctor_through_the_binding(tmp_path):
    import subprocess

    stack = str(tmp_path / "stack")
    switchyard.init(stack)

    with pytest.raises(TypeError):
        switchyard.release(stack, "alpha", True)
    with pytest.raises(TypeError):
        switchyard.release(stack, "alpha", keep_branch=1)
    with pytest.raises(TypeError):
        switchyard.doctor(stack, True)
    with pytest.raises(TypeError):
        switchyard.doctor(stack, fix=1)

    switchyard.claim(stack, "alpha")
    wt = tmp_path / "stack" / "worktrees" / "alpha"
    (wt / "dirty.txt").write_text("unsaved\n")

    with pytest.raises(switchyard.RefusalError):
        switchyard.release(stack, "alpha", safe=True)
    assert wt.is_dir()

    subprocess.run(["git", "-C", str(wt), "add", "dirty.txt"], check=True)
    subprocess.run(
        ["git", "-C", str(wt), "config", "user.email", "py@example.invalid"],
        check=True,
    )
    subprocess.run(["git", "-C", str(wt), "config", "user.name", "py"], check=True)
    subprocess.run(["git", "-C", str(wt), "commit", "-q", "-m", "saved"], check=True)

    with pytest.raises(switchyard.RefusalError):
        switchyard.release(stack, "alpha", safe=True)

    released = switchyard.release(stack, "alpha", safe=True, keep_branch=True)
    assert released["agent"] == "alpha"
    assert not wt.exists()

    (tmp_path / "stack" / ".tmp-stale").write_text("stale\n")
    audit = switchyard.doctor(stack)
    assert audit["ok"] is False
    assert audit["fixed"] == 0
    assert audit["kept_branches"] == ["switchyard/alpha"]
    assert [i["kind"] for i in audit["issues"]] == ["stale_temp_file"]
    assert audit["issues"][0]["fixed"] is False

    repaired = switchyard.doctor(stack, fix=True)
    assert repaired["ok"] is True
    assert repaired["fixed"] == 1
    assert repaired["kept_branches"] == ["switchyard/alpha"]
    assert repaired["issues"][0]["fixed"] is True
    assert not (tmp_path / "stack" / ".tmp-stale").exists()

    (tmp_path / "stack" / "switchyard-config.json").write_text("{corrupt config\n")
    with pytest.raises(switchyard.CorruptRegistryError):
        switchyard.list(stack)
    with pytest.raises(switchyard.CorruptRegistryError):
        switchyard.doctor(stack)
