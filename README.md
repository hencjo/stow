# stow

`stow` is a small GitOps reconciler for code-described service deployments on a Docker host. The GitLab deployment repository is the source of truth: each service is declared as a `stow.yaml` deployment descriptor plus its versioned configuration files.

It is deliberately small: GitLab merge requests are the change-control flow, and the host continuously converges actual runtime state toward the desired state in Git. `stow` downloads the service descriptor, decrypts SOPS-managed configuration, computes a content hash, starts Docker containers with matching labels, and rolls back automatically if the new service instance fails verification.

The usual workflow is:

1. An application pipeline builds and pushes a Docker image.
2. `stow suggest-image` opens or updates a GitLab merge request that pins the new image digest in the deployment repo.
3. After the MR is merged, a deployment-repo pipeline calls the host daemon.
4. `stow daemon` reconciles the Docker host to the merged Git commit.

In OTF-style terms, `stow` treats services as code: declarative service descriptors, desired-state reconciliation, immutable digest-pinned artifacts, versioned configuration, auditable approvals, convergence status, and boring rollback behavior in a small single-binary tool.

## suggest-image

`suggest-image` is the image-bump mode. It is meant to run from the application build pipeline after a successful Docker build.

It:

- reads the target deployment repo through the GitLab API
- loads `<subfolder>/stow.yaml`
- replaces the selected container image with the new image digest
- force-updates a fresh `suggest/...` branch from current default branch
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

Optional:

- `--digest`; if omitted, `stow` asks Docker for the digest
- `--container`; target container name (required)
- `--assign`; comma-separated MR assignment attempts, tried in order:
  `gitlab_user_id`, `id:<gitlab-user-id>`, `user:<gitlab-username>`
- `--changelog-file`; adds only the added markdown lines between old and new image tags

`--digest` must be the registry manifest digest. Use the `digest: sha256:...` line from `docker push`; Docker image IDs are local image/config digests rather than pullable registry manifest digests.

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
- `reconciling` means the daemon is actively applying that Git commit
- `queued` means that Git commit is queued behind another reconcile
- `different` means the host is running a deployment from another Git commit
- `error` means the last reconcile failed

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
sopsBinary: /usr/bin/sops
listen: 0.0.0.0:17403

tlsCrt: /etc/stow/tls.crt
tlsKey: /etc/stow/tls.key
```

Set `gitlabToken` in this root-only config file as the single token location.

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
- a desired container is `noop` only when it is running and both labels match
- a missing, stopped, or stale-hash container is replaced
- a labeled container no longer present in `stow.yaml` is deleted

Verification requires every desired container to exist, run, keep the expected labels, avoid restarts, avoid Docker's `Restarting` state, and report `healthy` if it has a healthcheck. The deployment must remain stable for 20 seconds inside a 60 second verification window.

## deploy and rollback cycle

A deployment is not considered complete when `docker run` exits. It is complete only after the new desired state has been applied and verified.

Normal deploy cycle:

```text
trigger received
  -> fetch desired Git revision
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
