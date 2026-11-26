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

## Public documentation and releases

Write the bundled skills and release notes for any authorized installation.
Keep personal names, device assignments, credential lookup details and private
team procedures in private configuration or documentation. Review the bundled
skill diff and release notes for these details before publishing.

When installing on a shared device bench, coordinate with the operators and
install between measurement rounds.

## Rust builds and tests

Use `cargo test` and `cargo build --release` to verify release changes. Use a
dedicated build machine when available for expensive builds and tests.

## Agent instruction files

`AGENTS.md` is canonical. `CLAUDE.md` and `GEMINI.md` are symlinks to it.
Restore the aliases with `ln -sf AGENTS.md CLAUDE.md && ln -sf AGENTS.md GEMINI.md`.
