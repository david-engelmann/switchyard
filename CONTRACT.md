# Contract

This contract states the setup obligations for switchyard first and the feature obligations of each build slice after them. Each obligation is stated so it can fail, and each one names how it is covered.

## Obligation S1

Obligation S1 covers seed integrity. The seed files README.md and LICENSE and .gitignore are present from the first commit, and later work lands through pull requests rather than as direct pushes to main. This is covered by inspection of the repository history and by the file presence tests in the tests folder.

## Obligation S2

Obligation S2 covers the local gate. The test suite runs with python and pytest alone and running it needs no network access and no paid service, and the continuous integration workflow runs the same commands as the local gate. This is covered by the tests in the tests folder and by the workflow file under .github/workflows.

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
