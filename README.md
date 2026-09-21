# stow

`stow` is a small GitOps reconciler for code-described service deployments on a Docker host. The GitLab deployment repository is the source of truth: each service is declared as a `stow.yaml` deployment descriptor plus its versioned configuration files.

It is deliberately small: GitLab merge requests are the change-control flow, and the host continuously converges actual runtime state toward the desired state in Git. `stow` downloads the service descriptor, decrypts SOPS-managed configuration, computes a content hash, starts Docker containers with matching labels, and rolls back automatically if the new service instance fails verification.

The usual workflow is:

1. An application pipeline builds and pushes a Docker image.
2. `stow suggest-image` opens or updates a GitLab merge request that pins the image digest and optional environment changes in the deployment repo.
3. After the MR is merged, a deployment-repo pipeline calls the host daemon.
4. `stow daemon` reconciles the Docker host to the latest default-branch HEAD.

In OTF-style terms, `stow` treats services as code: declarative service descriptors, desired-state reconciliation, immutable digest-pinned artifacts, versioned configuration, auditable approvals, convergence status, and boring rollback behavior in a small single-binary tool.

## suggest-image

`suggest-image` proposes image and environment updates from the application build pipeline after a successful Docker build. It neither merges nor deploys them.

It:

- reads the target deployment repo through the GitLab API
- loads `<subfolder>/stow.yaml` at the default branch's captured commit SHA
- edits only the selected image and requested environment values, preserving unrelated YAML bytes
- commits both changes atomically on the stable `suggest/...` branch, based on that SHA
- creates or updates a merge request
- sets the MR source branch to delete on merge
- adds a linked convergence badge if `deployment.daemonBaseUrl` exists in `stow.yaml`
- optionally adds changelog entries from a markdown file

Example:

```bash
CI_API_V4_URL=https://git.example.com/api/v4 \
GITLAB_ACCESS_TOKEN=... \
stow suggest-image \
  --project ops/deployments \
  --subfolder deploy-host.example.com \
  --image registry.example.com/apps/webapp:20260428.0 \
  --digest 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --container webapp \
  --assign gitlab_user_id,id:146,user:some.username \
  --changelog-file CHANGELOG.md
```

Required:

- `CI_API_V4_URL`
- `GITLAB_ACCESS_TOKEN` or `CI_JOB_TOKEN`
- `--project`
- `--subfolder`
- `--image`
- `--container`; explicit even for a single-container manifest

Optional:

- `--digest`; if omitted, `stow` asks Docker for the digest
- `--assign`; comma-separated MR assignment attempts, tried in order:
  `gitlab_user_id`, `id:<gitlab-user-id>`, `user:<gitlab-username>`
- `--env NAME=VALUE`; repeatable literal environment assignments
- `--release-version STRING`; display version, independent of the image tag and environment
- `--changelog-file`; local changelog path relative to the caller's working directory

`--digest` must be the registry manifest digest. Use the `digest: sha256:...` line from `docker push`; Docker image IDs are local image/config digests rather than pullable registry manifest digests.

Digests contain exactly 64 hexadecimal digits, optionally prefixed with `sha256:`,
and are normalized to lowercase. With `--digest`, no Docker or registry access is
needed. Without it, the existing Docker manifest-inspect/pull/inspect workflow applies.
Nonblank `GITLAB_ACCESS_TOKEN` takes precedence over `CI_JOB_TOKEN`; tokens are
trimmed. The fallback token still needs permission for the GitLab operations.
`suggest-image` does not accept `--config` and never contacts a host.

### Release and environment suggestions

Run this from the application's CI checkout (`stow-suggest` is the CI job name,
not another executable):

```sh
stow suggest-image \
  --project sigma/sigma-team-deployments \
  --subfolder abpm-0001.ext.tele2.com \
  --container kappa \
  --image nexus.int.tele2.com:18443/kappa/kappa:370d336 \
  --digest "$IMAGE_DIGEST" \
  --release-version 20260914.0 \
  --env RELEASE_VERSION=20260914.0 \
  --changelog-file kappa/CHANGELOG.md \
  --assign gitlab_user_id,user:henrsjos
```

`--env` splits at the first `=`. Names match `[A-Za-z_][A-Za-z0-9_]*`; values
preserve whitespace, extra `=`, Unicode, newlines and empty strings. NUL is
rejected. No shell, variable or template expansion occurs in Stow; quote values
in the calling shell, for example `--env 'LABEL=$HOME {{literal}}'`.
Repeated names use the last value while retaining their first argument position.
`--env EMPTY=` sets an empty string; there is no unset operation. Unspecified keys
remain untouched. **These values appear in Git and MR descriptions: this is not
a secret channel.** Keep using the existing secret-file mechanism for secrets.

`--release-version` accepts a nonblank display string without control characters;
the last supplied version wins. It does not set `RELEASE_VERSION`, change
`SOURCE_REVISION`, or build, retag or promote an image. Supply the environment
assignment separately. Its presence treats image tags as opaque and bypasses
numeric downgrade checks, even for numeric tags. Without it, the existing numeric
and git-describe checks still apply; `--env` alone does not bypass them.

With `--release-version`, the changelog quoted in the merge request is taken from
`--changelog-file` as the section for that version: the entries under a leading
`# Unreleased` heading (the gitlab/release.sh format, where those entries become the
release on promotion; the heading itself is not quoted), a headerless top block, or
a top heading ending in the version. Any other top heading is reported and skipped.

An environment-only suggestion still supplies the current `--image` and normally
its `--digest`. Existing 0.2 hosts consume the resulting manifest unchanged.
Real edits involving anchors/aliases anywhere in the selected container, or
container/environment merge keys, fail closed. Block and flow mappings are
supported; replaced env values must already be strings. Unrelated anchored
containers do not prevent an edit. Semantic no-ops preserve the original bytes.

### Changelogs and retries

Without a release version, different tags select added lines from local
`git diff --no-ext-diff --find-renames --unified=0 <old-tag>..<new-tag> -- <path>`.
Equal tags skip this changelog lookup. With a release version, Stow reads the
local file's top block: a first `# ` heading must end in the exact version token
(optional closing `#` markers are ignored), or the block may be headerless.
The next `# ` heading ends the block; `##` subsections and headings inside
backtick/tilde code fences do not. A mismatched heading is not searched past.
Missing files/refs, empty files and mismatches produce diagnostics, not failed
suggestions. The excerpt is shown as Markdown source; the file is never changed.

Already-matching image and final env values produce no commit or MR, even with a
different display version. This path does not close stale MRs. Identical pending
manifest bytes reuse their commit, and an open MR is updated rather than duplicated.
A commit may succeed before the MR request fails; rerun the same command safely.
If the target head moves during preparation, Stow fails with
`target branch changed while preparing suggestion; retry` before writing.
The recheck is best effort, not a lock or compare-and-swap guarantee.

Suggestion branches are automation-owned. Their name is `suggest/` followed by
`<subfolder>-<container>`, replacing every character outside ASCII letters,
digits, `-`, `_`, and `.` with `-`. Every proposal starts from the default branch,
not the previous suggestion: replacement can discard pending/manual edits.
Concurrent writers and sanitized-name collisions share a last-writer-wins branch.
Serialize CI jobs for each target/container when every suggestion must survive.
Image and environment updates share one file revision, commit and MR; the GitLab
API calls themselves are not a transaction.

### Pre-release compatibility gate

The source baseline is `stow-v0.2.2` (`0a7ba7d`), whose tree matches the starting
`main` (`8c62316`). Local tests cover the old loader, deployment hashing and Docker
environment handoff. Before releasing 0.3, confirm the actual deployed binary's
source, replay the consuming CI invocation with representative deployed manifests
against a scratch GitLab project, then merge and reconcile on that 0.2 host.
Verify idempotency before and after merge and confirm Kappa receives the release
version without overriding its image-built source revision. This live gate is
separate from local tests; no configuration migration or host upgrade is intended.

### Convergence badges

Put the daemon URL in the target `stow.yaml`:

```yaml
deployment:
  name: webapp
  daemonBaseUrl: https://deploy-host.example.com:17403/
```

Badge logic:

- `suggest-image` creates/updates the suggest branch first
- the MR badge points at the Git commit that contains the proposed `stow.yaml`
- the badge image is:
  ```text
  <daemonBaseUrl>/gitlab.svg?git_hash=<suggest-commit-sha>
  ```
- the badge links to:
  ```text
  <daemonBaseUrl>/status?head_hash=<suggest-commit-sha>
  ```
- the daemon resolves that Git commit to the expected deployment hash, then compares it with the host's current running deployment hash
- `running` means the host is running the deployment produced by that Git commit
- `reconciling` means the trigger associated with that Git commit is active while the daemon reconciles current default-branch HEAD
- `queued` means the trigger associated with that Git commit is the coalesced follow-up behind the active reconcile
- `different` means the host is running a deployment from another Git commit
- `error` means the last reconcile failed

## container environment

Set literal environment variables per container with an `env` map:

```yaml
containers:
  - name: webapp
    image: registry.example.com/apps/webapp:20260428.0@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    env:
      LOG_LEVEL: "info"
      PORT: "8080"
      OPTIONAL_VALUE: ""
```

Environment names must match `[A-Za-z_][A-Za-z0-9_]*`. Values must be YAML
strings and are passed to Docker exactly as written; quote numbers and
booleans. Host-variable inheritance, interpolation, and env files are not
supported. SOPS-encrypted values work through the normal decrypt-before-parse
flow. Plan JSON includes environment names but redacts every value, and Docker
receives values through the client process environment rather than command-line
arguments.

## daemon

`daemon` runs on the Docker host. It exposes a small HTTPS API for triggering reconcile and reading status.

Put daemon config in:

```text
/etc/stow/daemon.yaml
```

The whole config directory must be locked down:

- owned by `root`
- directories mode `0500`
- files mode `0400`
- regular files and directories only

Good:

```bash
sudo install -d -o root -g root -m 0500 /etc/stow
sudo install -o root -g root -m 0400 daemon.yaml /etc/stow/daemon.yaml
sudo install -o root -g root -m 0400 tls.crt /etc/stow/tls.crt
sudo install -o root -g root -m 0400 tls.key /etc/stow/tls.key
```

Daemon config format:

```yaml
gitlabBase: https://git.example.com/api/v4
project: ops/deployments
gitlabToken: glpat-...
subfolder: deploy-host.example.com

keys: /root/keys.txt
listen: 0.0.0.0:17403

tlsCrt: /etc/stow/tls.crt
tlsKey: /etc/stow/tls.key
```

Set `gitlabToken` in this root-only config file as the single token location.
Install `sops` on the daemon host's `PATH`; the config file no longer accepts a custom SOPS binary path.

Dry-run reconcile:

```bash
stow reconcile --config /etc/stow/daemon.yaml --dry-run --plan-json
```

Run daemon manually:

```bash
stow daemon --config /etc/stow/daemon.yaml
```

The daemon listens on HTTPS.

Rollout failure behavior is automatic:

- `stow` saves the previous running state before applying a new one
- after `docker run`, it waits up to 60 seconds for every desired container to become valid and stay valid for 20 seconds
- a container is valid when it exists, is running, is stable, has zero restarts, has the expected stow labels, and is `healthy` if it has a Docker healthcheck
- if apply or verification fails, `stow` restores the previous state and reapplies it

Intentional rollback should be done by reverting Git and letting the daemon reconcile that commit.

## reconcile loop

The reconcile loop is deliberately boring: fetch desired state, hash it, compare it with Docker, apply the delta, verify, then either commit the new runtime state or roll back.

```text
Git revision
  -> download deployment repo archive
  -> select configured subfolder
  -> decrypt SOPS files in place
  -> compute hashes
  -> load stow.yaml
  -> inspect Docker containers
  -> plan noop / replace / delete
  -> apply plan
  -> verify running containers
  -> keep new state or restore previous state
```

Deployment archives may contain only directories and regular files. PAX and
GNU extension metadata headers are ignored; links, devices, FIFOs, sparse
files, and other special entries are rejected before extraction.

Hashing is path-sensitive and content-sensitive. Files are walked in sorted order, and each hashed file contributes:

```text
relative/path + NUL byte + file contents
```

`stow` keeps four hashes in the plan output:

- `manifest_hash`: `stow.yaml` only.
- `config_hash`: all regular files except `stow.yaml` and decrypted secret files.
- `secrets_hash`: decrypted secret files only.
- `deployment_hash`: the combined hash of `manifest_hash`, `config_hash`, and `secrets_hash`.

The `deployment_hash` is the identity of the desired runtime state. It changes when:

- the container definition in `stow.yaml` changes
- an image tag or digest changes
- any non-secret config file changes
- any decrypted secret value changes
- a hashed file is renamed

It does not change because of Git metadata, commit message text, file mtimes, directory mtimes, Docker image IDs, or unreferenced containers on the host.

Runtime state transitions:

```text
downloaded repo
  -> staging directory
  -> ~/.stow/snapshots/<deployment_hash>
  -> ~/running-config symlink
```

On each successful apply:

1. The new staged config is moved to `~/.stow/snapshots/<deployment_hash>`.
2. The old `~/running-config` symlink is moved to `~/running-config.previous`.
3. `~/running-config` is pointed at the new snapshot.
4. Metadata is written into the running config:
   - `.git-revision`
   - `.config-sha256`
   - `.deployment-name`
   - `.stow-snapshot.json`
   - `.stow-rendered-manifest.yaml`
5. Docker containers are reconciled.
6. If verification passes, `running-config.previous` is removed.

If apply or verification fails, `stow` restores `running-config.previous`, reapplies that previous manifest, and verifies it. This is why rollback is local and fast: the previous snapshot is already on disk.

Docker reconciliation is label-based:

- every managed container gets `stow.deployment=<deployment name>`
- every managed container gets `stow.hash=v1:<deployment_hash>`
- `deployment.name` is the deployment identity and cannot change after the first successful deployment
- a desired container is `noop` only when it is running and both labels match
- a missing, stopped, or stale-hash container is replaced
- a labeled container no longer present in `stow.yaml` is deleted
- before stopping or removing a container, `stow` rechecks that its deployment label matches; an unmanaged name collision fails safely

Verification requires every desired container to exist, run, keep the expected labels, avoid restarts, avoid Docker's `Restarting` state, and report `healthy` if it has a healthcheck. The deployment must remain stable for 20 seconds inside a 60 second verification window.

## deploy and rollback cycle

A deployment is not considered complete when `docker run` exits. It is complete only after the new desired state has been applied and verified.

Normal deploy cycle:

```text
trigger received
  -> fetch current default-branch HEAD
  -> decrypt and hash desired state
  -> move desired state into running-config
  -> stop/remove containers that should change
  -> start replacement containers with stow labels
  -> verify all desired containers
  -> remove running-config.previous
  -> report success
```

`stow` decides a deployment is good when all desired containers pass the full verification window:

- the container exists
- it is running
- it is not in Docker's `Restarting` state
- it has restart count `0`
- it has `stow.deployment=<deployment name>`
- it has `stow.hash=v1:<deployment_hash>`
- if Docker reports a healthcheck, the health status is `healthy`
- the whole desired deployment stays valid for 20 continuous seconds
- this all happens before the 60 second verification timeout

If any condition fails, `stow` keeps waiting until the timeout. A container that briefly looks good and then restarts resets the stable timer.

Automatic rollback cycle:

```text
new deploy fails apply or verification
  -> move running-config.previous back to running-config
  -> load the previous manifest
  -> plan Docker back to the previous hash
  -> apply the rollback plan
  -> verify the previous deployment
  -> report the new deploy as failed
```

Rollback is therefore state rollback, not a best-effort container restart. The previous on-disk snapshot includes the previous manifest, config, decrypted secrets, Git revision metadata, and deployment hash. Docker is reconciled back to that snapshot using the same label and verification rules as a normal deploy.

Intentional rollback is simpler: revert the deployment repository, merge that revert, and trigger the daemon. To `stow`, that is just another desired Git revision with its own deployment hash.

## installing daemon with systemd

Copy the `stow` binary to the host first, normally:

```bash
sudo install -o root -g root -m 0755 stow /usr/local/bin/stow
```

Then upsert the service:

```bash
sudo stow install-systemd --config /etc/stow/daemon.yaml
```

This writes/updates:

- `/etc/systemd/system/stow.service`

Then it runs:

- `systemctl daemon-reload`
- `systemctl enable stow.service`
- `systemctl restart stow.service`

Check it:

```bash
systemctl status stow.service
journalctl -u stow.service -f
```

## triggering

Trigger a reconcile. Clients should use bounded retry with exponential backoff and jitter, especially from CI pipelines, so repeated webhook or network failures create gentle load instead of a request storm:

```bash
curl --fail --silent --show-error \
  --request POST \
  "https://deploy-host.example.com:17403/trigger?head_hash=<git-commit-sha>"
```

`head_hash` identifies the trigger for status and badge correlation; it does
not pin reconciliation to a caller-supplied revision. Every run fetches current
default-branch HEAD from GitLab. The daemon runs one reconcile at a time and
coalesces any number of triggers received while busy into one follow-up run.
There is no unbounded queue or concurrent reconcile fan-out, though a sustained
stream of triggers can keep scheduling successive follow-ups.

Check status:

```bash
curl --fail --silent --show-error \
  "https://deploy-host.example.com:17403/status?head_hash=<git-commit-sha>"
```

Badge URL:

```text
https://deploy-host.example.com:17403/gitlab.svg?git_hash=<git-commit-sha>
```

For the deployment repo, use `stow-merge-gitlab-ci.yaml` as the post-merge pipeline shape. It:

- runs on default branch
- detects which deployment directories changed
- reads each directory's `deployment.daemonBaseUrl`
- posts `/trigger?head_hash=$CI_COMMIT_SHA`
- leaves convergence reporting to the daemon status and badge endpoints

If the daemon uses a private CA, set:

```bash
STOW_CACERT=/path/to/ca.pem
```
