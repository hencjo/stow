---
name: release-plz-changelog
description: Use when making, reviewing, committing, or preparing PR/MR text for this repo so every contribution has a release-plz-compatible changelog entry and versioning stays driven by Conventional Commits.
---

# release-plz changelog discipline

This repo uses release-plz. `Cargo.toml` is the version source of truth, `flake.nix` reads from it, and `CHANGELOG.md` is updated by release-plz release PRs.

## Required workflow

For every contribution/MR, provide a changelog entry by using a release-plz-compatible Conventional Commit summary:

- `fix: ...` for bug fixes and patch-worthy behavior changes.
- `feat: ...` for user-visible features.
- `docs: ...`, `chore: ...`, `refactor: ...`, `test: ...`, or `ci: ...` for non-feature work that should still be auditable.
- Use `!` or a `BREAKING CHANGE:` footer for breaking changes.

## While editing

- Do not bump versions manually.
- Do not edit `CHANGELOG.md` for ordinary changes; release-plz owns generated changelog entries.
- If a user asks for a commit, PR, or MR, include a concise Conventional Commit title.
- If no commit is created, include a final-response line:
  `Changelog entry: <conventional-commit summary>`

## Good examples

- `fix: preserve container restart policy during reconcile`
- `feat: add GitHub release automation`
- `docs: document daemon trigger backoff`
- `ci: add release-plz workflow`
- `chore!: change release tag naming`

