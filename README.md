# switchyard

A worktree and port orchestrator that gives every parallel agent its own branch, port range, database, and env file with one command.

## What this is

Parallel agents collide where worktree tools stop, on ports, databases, and env files, and the best known worktree tool does no task assignment, no supervision, and no view inside the worktrees. Switchyard finishes the job for the full stack, provisioning the complete isolated environment per agent with one command. Each agent gets a real stack and a live URL instead of a bare directory, and the operator sees which environments are working and which are waiting. The boundary with its siblings is one sentence. Mayfly makes the box, switchyard furnishes it.

## Status

The remote seed was verified on 2026-11-02 against the staged seed and its commit is e7e7d53dcd4d41fe5b46d0c51f1808872ff133fb. All work arrives through pull requests and nothing lands on main directly. The contract lives in CONTRACT.md and carries the setup obligations S1 to S5 and the Round 1 feature obligations SY-1 to SY-10. Round 1 builds the first working slice, one claim command that furnishes a full stack environment per agent, with the live view as a printed table and a release that returns everything. The Rust port that opens Round 2 replaces the Python implementation with a crate that reproduces the Round 1 behaviour, with the differences named in the port amendment of CONTRACT.md. Closed perfection rounds are 2 of 100. A round closes only when its full cycle of work and audit and review and red team and approval has run. The local gate is cargo test plus the codepoint scan run through cargo, with cargo fmt --check and cargo clippy --all-targets -- -D warnings.

## Use

A stack is one directory holding a git repository, the worktrees, and one registry file that records the port pool and every live claim. The registry is the only record of who holds what, and the list command reads nothing else.

```bash
switchyard init --stack ./stack
switchyard claim alpha --stack ./stack
switchyard list --stack ./stack
switchyard release alpha --stack ./stack
```

A claim creates a worktree on a fresh branch and draws one port range from the pool recorded in the registry and derives a database name from the agent name and writes an env file into the worktree. Claims and releases hold one file lock across the whole read and check and write of the registry, so processes racing for the same name or for ranges cannot both win. A release removes the worktree and the env file inside it and deletes the branch and returns the range to the pool, and releasing is destructive, because the worktree goes away with whatever the agent left uncommitted in it. A corrupt registry stops every command with a loud error while nothing on disk changes, and releasing an agent that holds no claim is an error rather than a quiet success.

## Python binding

The crate carries a thin Python binding for the orchestrators that furnish runs from Python. The binding lives in the bindings folder as its own crate and wraps the same library the command line wraps, so a claim made through Python and a claim made through the binary land in the same registry and follow the same rules. Build it into a virtual environment with maturin.

```bash
python -m venv .venv
. .venv/bin/activate
pip install maturin
maturin develop --manifest-path bindings/Cargo.toml
```

The switchyard package exposes the four lifecycle operations as the functions init and claim and release and list. Each takes the stack path as a string or a path and the agent name where the operation names one, and each returns plain dictionaries carrying the facts the command line prints, with init returning the port pool and claim and release returning the claim record and list returning one record per live claim. A refusal raises a subclass of SwitchyardError carrying the same message text the command line prints, and the subclass names the class of the refusal, with RefusalError for a plain refusal and NotInitialisedError for an uninitialised stack and CorruptRegistryError for a corrupt registry and InvalidNameError for an agent name the library rejects.

## Licence

MIT. See LICENSE.
