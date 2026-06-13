# How to release stow

This repo uses release-plz for version bumps, changelog updates, Git tags, and GitHub releases.

`Cargo.toml` is the version source of truth. `flake.nix` reads the version from `Cargo.toml`.

## One-time GitHub setup

In the GitHub repository settings:

1. Go to **Settings → Actions → General**.
2. Under **Workflow permissions**, choose **Read and write permissions**.
3. Enable **Allow GitHub Actions to create and approve pull requests**.

No crates.io token is needed. This repo uses `git_only = true`, so release-plz creates GitHub tags/releases and skips `cargo publish`.

## Everyday development

Use Conventional Commit-style summaries for changes that should be clear in the changelog:

- `fix: handle missing state metadata`
- `feat: add daemon status endpoint`
- `docs: clarify release process`
- `ci: upload release binary artifacts`
- `chore: update dependencies`
- `feat!: change manifest schema`

Do not manually bump versions for normal changes.
Do not manually edit generated changelog entries for normal changes.

## Cutting a release

Pushing normal work to `main` does **not** publish a release by itself. It should open or update a release PR. The release is published only after that PR is merged.

1. Merge normal work to `main`.
2. Wait for the **Release-plz PR** workflow to open or update a release PR.
3. Review the release PR:
   - `Cargo.toml` version bump looks right.
   - `CHANGELOG.md` entries are useful.
   - The release title/version is what you expect.
4. Merge the release PR.
5. Wait for **Release-plz release** to create the Git tag and GitHub release.
6. Wait for **Release artifacts** to attach the Linux static binary tarball and checksum.

The expected release artifact names look like:

```text
stow-<version>-linux-x86_64-static.tar.gz
stow-<version>-linux-x86_64-static.tar.gz.sha256
```

## Manual artifact rebuild

If the release exists but artifacts need to be rebuilt:

1. Go to **Actions → Release artifacts**.
2. Click **Run workflow**.
3. Enter the tag, for example:

```text
stow-v0.1.0
```

The workflow rebuilds the artifact from that tag and uploads it to the matching GitHub release with `--clobber`.

## Local sanity checks

Before merging release-related changes, run:

```bash
devenv shell cargo test
git diff --check
nix eval .#packages.x86_64-linux.stow.version --raw
```

The Nix version should match `Cargo.toml`.

## If release-plz does something surprising

If pushing to `main` does not open a release PR, check the **Release-plz PR** log. In git-only mode, release-plz must find the previous tag using `git_tag_name`. This repo uses tags like:

```text
stow-v0.1.0
```

So `release-plz.toml` must keep:

```toml
git_tag_name = "{{ package }}-v{{ version }}"
git_release_name = "{{ package }}-v{{ version }}"
```

Without that, release-plz looks for the default single-crate pattern `v0.1.0`, treats the repo as an initial release, and may decide there is nothing to do.

Check these files first:

- `release-plz.toml`
- `.github/workflows/release-plz.yml`
- `.github/workflows/release-artifacts.yml`
- `Cargo.toml`
- `CHANGELOG.md`

Keep the split clear: release-plz manages versions, changelog, tags, and GitHub releases; the artifact workflow builds and uploads binaries.
