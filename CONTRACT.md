# Contract

This contract states the setup obligations for switchyard first and the feature obligations of each build slice after them. Each obligation is stated so it can fail, and each one names how it is covered.

## Obligation S1

Obligation S1 covers seed integrity. The seed files README.md and LICENSE and .gitignore are present from the first commit, and later work lands through pull requests rather than as direct pushes to main. This is covered by inspection of the repository history and by the file presence tests in the tests folder.

## Obligation S2

Obligation S2 covers the local gate. The test suite runs with cargo test plus the codepoint scan run through cargo, and running it needs no paid service, and the continuous integration workflow runs the same commands as the local gate. This wording is amended by the Rust port that opens Round 2. The setup wording named python and pytest alone and no network access, and the amendment replaces it under the program language ruling of 2026-11-02, which made every repo in the program a Rust crate. The amended gate keeps the no paid service term. It drops the no network access term for one stated reason, a first build fetches the crate dependencies from the public crates.io registry, and the tests themselves then run with no network access. This is covered by the tests in the tests folder and by the workflow file under .github/workflows.

## Obligation S3

Obligation S3 covers honest status. The README status section records the seed state and the contract state and the closed round count truthfully, and closed perfection rounds are 0 of 100 until a full cycle of work and audit and review and red team and approval has closed. This is covered by the status tests in the tests folder and by inspection against this contract.

## Obligation S4

Obligation S4 covers synthetic fixtures only. Every fixture and example in this repository is synthetic and carries no real trace and no real transcript and no name of a real person and no private operational detail. This is covered by inspection of the repository contents and by a sanitization scan before any push.

## Obligation S5

Obligation S5 covers canonical AGENTS identity. The part of AGENTS.md above the marked line is identical to the canonical template for the program and only the annex below that line differs per repo. This is covered by tests in the tests folder that pin the SHA-256 digest of the part above the marked line, and by comparison against the canonical template when this setup is accepted.

## Round 1 preamble, probe findings

The feature obligations below rest on behaviour probed against the real tools on 2026-11-02, with git 2.43.0 and Python 3.12.3. A worktree add with a branch flag creates the worktree on a new branch in one step. A worktree remove refuses a worktree that holds untracked or modified files and exits 128, and the env file a claim writes is untracked by construction, so release removes worktrees with the force flag and release is destructive by design. An exclusive flock lock on a lock file inside the stack root serialises the read and check and modify and write of the registry across processes on this platform. The registry is saved by rename from a sibling temporary file, so a reader never sees a half written registry.

## Obligation SY-1

Obligation SY-1 covers complete allocation. A successful claim creates a git worktree on a new branch for the agent and draws one port range from the pool recorded in the registry and derives a database name from the agent name and writes an env file inside the worktree, and the registry record for the agent matches the branch and the range and the database and the paths that exist on disk, and the record carries claimed_at, the time the claim was made. A claim that creates some of these and not others, or records values that differ from what it created, fails this obligation. This is covered by the claim tests in the tests folder, which read the registry and the disk and the repository after a claim and compare all three.

## Obligation SY-2

Obligation SY-2 covers one claim per agent. A second claim for an agent that already holds a claim fails with a nonzero exit, and the registry and the worktrees stand exactly as the first claim left them. This is covered by the duplicate claim test in the tests folder, which compares the registry bytes and the worktree listing before and after the rejected claim.

## Obligation SY-3

Obligation SY-3 covers the port pool. The port ranges of live claims never overlap, and when every range in the recorded pool is held a further claim fails with a nonzero exit and records nothing. This is covered by the pool tests in the tests folder, which fill a small pool and compare the claimed ranges pairwise and then attempt one claim too many.

## Obligation SY-4

Obligation SY-4 covers atomicity under concurrency. Claim and release hold one exclusive file lock on the stack lock file across the whole read and check and modify and write of the registry, so processes racing the same agent produce exactly one success, and processes racing distinct agents never take overlapping ranges. A build that checks the registry and writes it in separate unlocked steps fails this obligation. This is covered by the concurrency tests in the tests folder, which race real parallel processes against one registry behind a shared start barrier.

## Obligation SY-5

Obligation SY-5 covers the list view. The list command derives its table only from the registry and prints exactly one row for each recorded claim, and a worktree or directory that exists on disk without a registry entry never appears in the table. This is covered by the list tests in the tests folder, which plant an unrecorded directory inside the stack and check that the table ignores it.

## Obligation SY-6

Obligation SY-6 covers complete release. A release removes the worktree directory and the env file inside it and deletes the claim branch and returns the port range to the pool and removes the registry entry, and after a release none of those artifacts remains, and a later claim can take the very range the release freed. This is covered by the release tests in the tests folder, which inspect the disk and the repository and the registry after a release and then claim again.

## Obligation SY-7

Obligation SY-7 covers unknown release. Releasing an agent that holds no claim fails with a nonzero exit and an error that names the agent, and the registry and the disk stand unchanged. This is covered by the unknown release test in the tests folder, which compares the registry bytes before and after.

## Obligation SY-8

Obligation SY-8 covers a corrupt registry. When the registry cannot be parsed or its shape is wrong, claim and list and release each fail with a nonzero exit and a loud error, and no file anywhere in the stack changes, because every command validates the registry before it touches anything else. This is covered by the corrupt registry tests in the tests folder, which snapshot every file in the stack and compare the snapshot after each rejected command.

## Obligation SY-9

Obligation SY-9 covers an uninitialised stack. When the stack root holds no registry, claim and list and release each fail with a nonzero exit and create nothing, and only the init command creates a registry. This is covered by the uninitialised stack test in the tests folder, which points every command at an empty directory and finds it still empty afterwards.

## Obligation SY-10

Obligation SY-10 covers agent name safety. A claim rejects an agent name that is not one path safe component of lowercase letters and digits and hyphens of at most 40 characters, so no claim can create a worktree or a file outside the stack root, and a rejected name changes nothing. This is covered by the name tests in the tests folder, which try traversal names and names with spaces and check that nothing appears inside or outside the stack.

## How the stub grew

The setup obligations above were the whole contract when this repo was seeded, and they said the full contract would be the first build act. Round 1 is that act. The setup obligations stand unchanged, the feature obligations SY-1 to SY-10 extend them, and from here the contract grows by amendment with the earlier text left standing.

## Amendment note, the Round 1 framing edits

The Round 1 amendment changed two pieces of framing text from the setup stub, and the record names both here because the earlier wording did not survive in place. The stub intro opened by calling this file a stub contract that states the setup obligations for switchyard, and it now says the contract states the setup obligations first and the feature obligations of each build slice after them. The stub closed with a section headed What this stub is not, which said the stub is not the full contract and that build work would extend it with the obligations of the first slice before any feature code lands, and that section was retired and replaced by the section above headed How the stub grew. No obligation text changed in either edit.

## Port amendment, the Rust conversion opening Round 2

Amendment, recorded with the Rust port that opens Round 2. The obligations SY-1 to SY-10 above stand unedited, and the implementation that carries them changes. The Python package of Round 1 is replaced by a Rust crate. The library carries the registry and its validation, the claim lifecycle, the worktree provisioning, the port allocation, and the env file writing. The switchyard binary carries the command line. The codepoint scanner is ported from the fleet template in Rust. Obligation S2 is amended inside its own paragraph above, in the form the conversion of deadbolt set, with the program language ruling of 2026-11-02 named as its reason. The conversion adds a thin Python binding over the crate as this round's second slice, landed separately from this port, because the orchestrators that furnish runs are often Python programs and a binding lets them drive the library in process. This port landing contains no binding code, and the library it lands is the crate that binding will wrap.

The equivalence standard for the port is threefold. The Round 1 test suite is translated pin for pin into the Rust suite. Registry documents written by the Python implementation are checked in as fixtures, and the Rust suite loads each one and operates on it and writes documents that match the documents the Python implementation writes for the same operations, byte for byte apart from the claim timestamps a live run cannot help but differ in. And a fixed battery of command shapes is driven side by side under the Python implementation from main and under the Rust binary, with exit codes and output and the resulting registry documents compared shape by shape, and the comparison is recorded in the gate report of the port pull request. A behaviour the translation cannot carry in its original shape is named below rather than dropped quietly.

Three reshapings are named here, because the obligations and the observable behaviour are the spec and the shape of the Python code is not. The Python exception hierarchy becomes one error type carrying the same messages and the same exit codes, because Rust has no exception hierarchy to translate it into and the exit code is the behaviour the command line owes. The registry reader is a strict reader over the parsed document that rejects a repeated key and an unrecorded field and a wrong shape with the same corrupt registry reports the Python validator raised and in the same order, because the format the two implementations share is the document and not the parser. The entry point a caller ran as a Python module becomes the switchyard binary itself, because a Rust crate ships binaries rather than modules, and the pin that drove the module now drives the binary.

Two mechanism adaptations are named here, both in the test suite. Three Python tests patched the private git runner and the private registry save and the process environment to stage failures no real stack offers, and the Rust suite stages the same failures against the same private seams from inside the crate, because the Python tests reached into private functions in exactly that way and the substance of each pin is unchanged. One Python test staged a failed registry save by patching the rename, and the Rust pin stages the same failure by standing a directory where the registry file belongs, so the write lands and the rename fails against a real file system.

Two differences are named here rather than smoothed over. The detail text of a JSON syntax error inside a corrupt registry report comes from the Rust parser and differs from the wording of the Python parser, while the classification and the exit code and the guarantee that nothing changes are identical, and the deeply nested document keeps the Python sentence of the report. And the timestamp a claim record carries is validated against the extended ISO 8601 shape the implementation itself writes, with a basic format date also accepted, where the Python validator accepted the full grammar of its ISO parser, so a hand written registry carrying an exotic timestamp form the implementation never writes can read as corrupt here where it read as sound there.

The three reviews of the port head found one defect the translation introduced and two it inherited from the Python behaviour it translates, and a repair on this branch closes all three before the round moves on. The pool end is now computed at a width the caller's numbers cannot overflow, and a magnitude that cannot stand is refused by the same reports Python raised, exit 1 from init as a pool error and a corrupt registry at exit 2 from every other command. The first Rust build computed the same end in the recorded width, where a debug build panicked and a release build wrapped and could persist an absurd pool. The Python suite is also recounted in this record, because the port's first account of it carried a wrong split. Collection on main counts 69 protocol items and 18 setup items, 87 in all, the translation covers every one of the 69, and the earlier figure of 67 protocol items stands corrected here.

The registry publish is re-decided in the same repair. A save no longer writes to the fixed temporary name the first build shared with Python. Each save writes to a sibling temporary file whose name carries a random suffix drawn for that save alone and is created so a name already standing is never opened, it renames that file over the registry path as before, and it removes its own temporary file on every failure path. The fixed name was a plant target. A link left standing at it made the publish write through the link and destroy the file the link pointed at, and the rename then left the registry path itself standing as that link. The registry document, the rename publish, and the guarantee that a failed save changes nothing are unchanged, and the retired name is no longer watched by init or cleaned up after a failed init, because nothing reads or writes it any more.

The stack lock is re-decided with it, as a file identity rather than a path name. A command that takes the lock confirms the file it holds is the file the lock path still names, by device and inode, once after taking the lock and again immediately before it publishes the registry, and on a mismatch it aborts with an error that names the lost lock, changes nothing on disk, and rolls its claim back by the rollback path every failed claim takes. The path alone could not see a one file tamper. Deleting the lock file while a claim held it let the next claim lock a fresh file at the same path, read the registry as the first claim had left it, and publish, and the first claim then saved over the second claim's record and left two standing claims on one range with one record between them. A lock whose file was replaced under its holder excludes nobody, and no registry is written under one.

One behaviour is carried unchanged as a named residual of this round. A claim killed after its worktree and branch are created and before its registry save leaves the branch standing, because the rollback preserves a branch it cannot prove the claim created, and later claims for that agent then fail at the worktree add until an operator removes the leftover branch and worktree by hand. The conservative refusal to delete what the claim cannot prove it made stands here, and the manual removal just named is the recovery.

Two smaller differences are recorded for completeness. The command line takes exact option names, where the Python parser accepted an unambiguous prefix of an option name as well, and a name the parser does not know is refused at exit 2 by both. And a git missing from the path is a clean one line error here at exit 1, where Python let the failure escape as a traceback at the same exit 1.
