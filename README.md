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
