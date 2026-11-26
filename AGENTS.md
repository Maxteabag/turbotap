# Turbotap repository instructions

## Pull requests and releases

Starting with PR #4, use the verified personal email on the Maxteabag GitHub
account for squash merges. The author email on the `v0.5.0` release commit is
the reference. GitHub may use the account's display name on a squash commit.
That name is acceptable when the author email matches the reference and GitHub
attributes the commit to Maxteabag. No extra approval is required for that
display name. Preserve the PR branch when squash merging.

For local commits, including version bumps, use `Maxteabag` as the Git author
name and the same personal email as the Git author email. Never use an ECIT work
email for either a local commit or a GitHub squash merge. Verify both identities
from the resulting commits.

## Installing a release while the phone team is measuring

Before installing a new turbotap on a machine that drives the test iPhone (the
EliteBook, laptopstudio), send one line to `correctness-loop` and `rivals` with
the version, and install between their measurement rounds. A binary that changes
in the middle of a bench round mixes two builds in its rows and voids the round
(26 Nov 2026: the v1.5.0 install at 05:01 voided phone-r12's `scrollabove`).
If neither answers within 10 minutes, install and say so.

Release lines, PR hand-offs and status go to `teabag`. Anything meant for Hazel
(coordination, questions, real blockers only she or Peter can solve) goes to
`hazel-ops`, her fork, never to `hazel-74f6`: Peter chats with Hazel there.

## Rust build host

Run all Rust builds and Cargo tests on `ai-swarm` through `buildbox-cargo`.
This applies to development, review and release verification:

```sh
buildbox-cargo <checkout> test
buildbox-cargo <checkout> build --release
```

Keep heavy work off EliteBook, laptopstudio and xps15. If the buildbox is
unavailable, use a durable continuation or report the blocker.

## Agent instruction files

`AGENTS.md` is canonical. `CLAUDE.md` and `GEMINI.md` are symlinks to it.
Restore the aliases with `ln -sf AGENTS.md CLAUDE.md && ln -sf AGENTS.md GEMINI.md`.
