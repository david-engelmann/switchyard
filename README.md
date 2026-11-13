# switchyard

Switchyard furnishes isolated workspaces for parallel agents, each a worktree on its own branch with its own port range, database name, and env file, and it routes claims by job description and queues the claims that must wait.

Switchyard is a worktree and port orchestrator carried by a Rust library with a command line and a thin Python binding. One stack directory holds a git repository, the worktrees, and one registry file that records the port pool and every standing claim and wait. A claim creates the worktree and the branch, draws the port ranges, derives the database name, and writes the env file in one operation under one file lock, and a release returns everything and furnishes any waiting claim the freed ranges can serve.

[![CI](https://github.com/david-engelmann/switchyard/actions/workflows/ci.yml/badge.svg)](https://github.com/david-engelmann/switchyard/actions/workflows/ci.yml)

```
AGENT  BRANCH            PORTS        DATABASE          WORKTREE
alpha  switchyard/alpha  20000-20009  switchyard_alpha  worktrees/alpha
```

## Install

Switchyard builds from source with Cargo. Clone the repo and install the binary onto your path.

```
git clone https://github.com/david-engelmann/switchyard.git
cd switchyard
cargo install --path . --bin switchyard
```

The install puts the switchyard binary in the Cargo bin directory. On a default install that directory is ~/.cargo/bin, and it must stand on your PATH for the switchyard command to resolve. The command names the switchyard binary on purpose. The repo also builds a codepoint_scan helper, and a machine that already carries that helper from a sibling repo refuses a plain install, so the named form installs switchyard alone.

## Quickstart

Create a stack, furnish one agent, and return everything.

```
switchyard init --stack ./stack
```

```
initialised stack at /demo/stack
port pool 16 ranges of 10 ports from 20000
```

```
switchyard claim alpha --stack ./stack
```

```
claimed alpha
branch switchyard/alpha
worktree worktrees/alpha
ports 20000-20009
database switchyard_alpha
env file worktrees/alpha/.env.switchyard
```

```
switchyard list --stack ./stack
```

```
AGENT  BRANCH            PORTS        DATABASE          WORKTREE
alpha  switchyard/alpha  20000-20009  switchyard_alpha  worktrees/alpha
```

```
switchyard release alpha --stack ./stack
```

```
released alpha
ports 20000-20009 returned
```

## Usage

The examples below run in one directory. The profile and routed examples use the quickstart's stack, and the queue example builds a small stack of its own, so every line of output follows from the commands as written.

### Profiles

A plain claim draws one port range. A claim under a declared profile draws the profile's ranges, one for lean, two for standard, and four for roomy.

```
switchyard claim beta --stack ./stack --profile standard
```

```
claimed beta
branch switchyard/beta
worktree worktrees/beta
ports 20000-20019
database switchyard_beta
env file worktrees/beta/.env.switchyard
```

### The wait queue

A claim whose draw does not stand free refuses, unless the claim carries the wait option. A waiting claim creates nothing and takes no port span. It stands in the queue until a release furnishes it or its bound passes. This run uses a small pool of two ranges so the queue forms at once.

```
switchyard init --stack ./small --range-count 2
switchyard claim alpha --stack ./small
switchyard claim beta --stack ./small --profile standard --wait
```

```
initialised stack at /demo/small
port pool 2 ranges of 10 ports from 20000
claimed alpha
branch switchyard/alpha
worktree worktrees/alpha
ports 20000-20009
database switchyard_alpha
env file worktrees/alpha/.env.switchyard
waiting beta
profile standard
score none
fallback no transport
```

The standard profile draws two ranges and only one stands free, so beta waits. With no judge command named, the wait stands scoreless and orders by arrival among the scoreless. A plain claim that carries the wait option still allocates at once where its own draw stands free.

```
switchyard claim gamma --stack ./small --wait
switchyard list --stack ./small --queue
```

```
claimed gamma
branch switchyard/gamma
worktree worktrees/gamma
ports 20010-20019
database switchyard_gamma
env file worktrees/gamma/.env.switchyard
AGENT  PROFILE   SCORE  RECORDED
beta   standard  none   2026-11-08T17:06:54.771778+00:00
```

A release serves the standing waits in offer order before it publishes, furnishing each wait whose whole draw stands free in the state being built and skipping the rest without splitting a draw. Alpha's release frees one range, which is not enough for beta. Gamma's release frees the second, and beta is furnished inside that release.

```
switchyard release alpha --stack ./small
switchyard release gamma --stack ./small
switchyard list --stack ./small
```

```
released alpha
ports 20000-20009 returned
released gamma
ports 20010-20019 returned
AGENT  BRANCH           PORTS        DATABASE         WORKTREE
beta   switchyard/beta  20000-20019  switchyard_beta  worktrees/beta
```

A wait can carry a bound in seconds. A bounded wait that no release furnishes is swept as expired at the head of the next claim or release after its bound passes, and releasing a waiting agent cancels its wait.

```
switchyard claim delta --stack ./small --wait --wait-for 600
switchyard release delta --stack ./small
```

```
waiting delta
profile plain
score none
fallback no transport
wait bound 600 seconds
released delta
wait cancelled
```

### Routed claims

A routed claim describes the job in prose and a decision model settles which profile the claim takes, landing on a declared default where the consultation does not decide. The consultation runs through one judge command the operator names, which receives the request document on its standard input and answers with the reply body on its standard output. The judge below is a stub that always settles standard, standing in for the decision model a real stack names.

```
cat > judge.sh <<'EOF'
#!/bin/sh
cat > /dev/null
printf '%s' '{"model":"jev-1.13.0","answers":{"profile":{"type":"choice","choice":"standard","probabilities":{"lean":0.05,"standard":0.9,"roomy":0.03,"other":0.02}}}}'
EOF
chmod +x judge.sh
switchyard claim epsilon --stack ./stack --describe "Run the api server and its worker and a test watcher side by side" --default-profile lean --judge-command ./judge.sh
```

```
claimed epsilon
branch switchyard/epsilon
worktree worktrees/epsilon
ports 20020-20039
database switchyard_epsilon
env file worktrees/epsilon/.env.switchyard
```

### The Python binding

The crate carries a thin Python binding for the orchestrators that furnish runs from Python. The binding lives in the bindings folder as its own crate and wraps the same library the command line wraps, so a claim made through Python and a claim made through the binary land in the same registry and follow the same rules. Build it into a virtual environment with maturin.

```
python -m venv .venv
. .venv/bin/activate
pip install maturin
maturin develop --manifest-path bindings/Cargo.toml
```

The switchyard package exposes the lifecycle operations as the functions init and claim and release and list, the queue read as the function queue, and the routing operations as the functions claim_profile and claim_routed. The claim functions take wait and wait_for arguments beside the landed arguments, and a bound named without the wait argument is refused. Each function takes the stack path as a string or a path and the agent name where the operation names one, and each returns plain dictionaries carrying the facts the command line prints, with init returning the port pool, claim and release returning the claim record or the wait record where the claim waited or the release cancelled one, list returning one record per live claim, and queue returning one record per standing wait in offer order. A refusal raises a subclass of SwitchyardError carrying the same message text the command line prints, and the subclass names the class of the refusal, with RefusalError for a plain refusal and NotInitialisedError for an uninitialised stack and CorruptRegistryError for a corrupt registry and InvalidNameError for an agent name the library rejects.

## Status

What works. Every command in the contract runs against the registry file. A claim furnishes a worktree on a fresh branch with a drawn port range, a derived database name, and an env file, and claims and releases hold one file lock across the whole read and check and write of the registry, so processes racing for the same name or for ranges cannot both win. A profiled claim draws the declared profile's ranges, and a routed claim settles its profile through the judge command against its declared default. A claim whose draw does not stand free can wait in the queue, scored once where a judge is named for the scoring question, and a release furnishes the standing waits it can serve in offer order. A corrupt registry stops every command with a loud error while nothing on disk changes, and releasing an agent that holds no claim and no wait is an error rather than a quiet success.

What does not. The judge is one operator supplied command per consultation, bounded by a thirty second deadline, and the repo ships no judge of its own. With no judge named, a routed claim lands on its declared default and a waiting claim stands scoreless. Expiry is lazy. A bounded wait is swept at the head of the next claim or release, never by a timer or a daemon, and nothing watches the queue between commands. A release is destructive, because the worktree goes away with whatever the agent left uncommitted in it.

What is not built. Claims do not expire. A claim stands until its agent is released, and only waits carry a bound. There is no network service and no supervision inside the worktrees a claim furnishes, and one stack serves the callers that share its filesystem. The comparison decision model named in ROADMAP.md is not wired, and the only judge route is the operator's own command.

The contract lives in CONTRACT.md and carries the setup obligations S1 to S5 and the Round 1 feature obligations SY-1 to SY-10 and the Round 3 routing amendment adding obligations SY-11 to SY-16 and the Round 4 wait queue amendment adding obligations SY-17 to SY-24 and the Round 5 hardening amendment adding obligation SY-25. The registry document and the routing and queue logs are contract surfaces specified there, and the registry shape the retired Python implementation wrote is carried in tests as fixtures the Rust suite reads. The Rust test suite carries 159 root tests across every operation and the Python binding smoke test carries 11, and both suites pass on the current head with the binding built the way CI builds it.

Closed perfection rounds are 48 of 100. A round closes only when its full cycle of work and audit and review and red team and approval has run. The program began as a seed commit whose state is independently verifiable against e7e7d53dcd4d41fe5b46d0c51f1808872ff133fb.

## License

MIT. See [LICENSE](LICENSE).
