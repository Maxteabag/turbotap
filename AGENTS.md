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
