# switchyard

A worktree and port orchestrator that gives every parallel agent its own branch, port range, database, and env file with one command.

## What this is

Parallel agents collide where worktree tools stop, on ports, databases, and env files, and the best known worktree tool does no task assignment, no supervision, and no view inside the worktrees. Switchyard finishes the job for the full stack, provisioning the complete isolated environment per agent with one command. Each agent gets a real stack and a live URL instead of a bare directory, and the operator sees which environments are working and which are waiting. The boundary with its siblings is one sentence. Mayfly makes the box, switchyard furnishes it.

## Status

Seeded in October 2026. The build has not started. Part of the impatient programming portfolio, in the Fleet Operations arc.

## Licence

MIT. See LICENSE.
