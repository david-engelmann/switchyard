# Contract

This stub contract states the setup obligations for switchyard. Each obligation is stated so it can fail, and each one names how it is covered.

## Obligation S1

Obligation S1 covers seed integrity. The seed files README.md and LICENSE and .gitignore are present from the first commit, and later work lands through pull requests rather than as direct pushes to main. This is covered by inspection of the repository history and by the file presence tests in the tests folder.

## Obligation S2

Obligation S2 covers the local gate. The test suite runs with python and pytest alone and needs no network access and no paid service, and the continuous integration workflow runs the same commands as the local gate. This is covered by the tests in the tests folder and by the workflow file under .github/workflows.

## Obligation S3

Obligation S3 covers honest status. The README status section records the seed state and the contract state and the closed round count truthfully, and closed perfection rounds are 0 of 100 until a full cycle of work and audit and review and red team and approval has closed. This is covered by the status tests in the tests folder and by inspection against this contract.

## Obligation S4

Obligation S4 covers synthetic fixtures only. Every fixture and example in this repository is synthetic and carries no real trace and no real transcript and no name of a real person and no private operational detail. This is covered by inspection of the repository contents and by a sanitization scan before any push.

## Obligation S5

Obligation S5 covers canonical AGENTS identity. The part of AGENTS.md above the marked line is identical to the canonical template for the program and only the annex below that line differs per repo. This is covered by the AGENTS tests in the tests folder and by comparison against the canonical template when this setup is accepted.

## What this stub is not

The full contract is the first build act of this repo. This stub is not the full contract, and build work extends it with the obligations of the first slice before any feature code lands.
