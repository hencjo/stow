# FIXME

Verdict: good core idea, but not clean/ship-ready yet. Biggest problem: the repo is lying about its tests.

Checks run:
- `devenv shell cargo check` ✅
- `devenv shell cargo test` ✅ 107 passed, but only because of an untracked file
- `devenv shell cargo fmt -- --check` ✅
- `devenv shell cargo build --release` ✅ static musl binary
- `devenv shell cargo clippy -- -D warnings` ❌ mostly cleanup/refactor lints

## Findings

1. **Critical: tests depend on untracked `src/test_support.rs`.**  
   `src/main.rs:15-16` declares `mod test_support`, but `src/test_support.rs` is not git-managed. Fresh clone = broken tests. Add it to git.

2. **Fixed: GitLab archive downloads no longer leak tokens via process list.**  
   Archive downloads now use in-process `ureq` requests with headers set directly on the request, so `PRIVATE-TOKEN` / `JOB-TOKEN` values are not passed as subprocess arguments.

3. **High: Docker image tag parsing is wrong for registry ports.**  
   `src/manifest.rs:224`, `243`, `347-352` treat any colon as a tag separator. `localhost:5000/api@sha256:...` wrongly passes as “tagged”. Parse the colon only after the last `/`.

4. **High: daemon HTTP API has no auth.**  
   It binds `0.0.0.0:17403` by default and exposes status/trigger/badge. That’s too trusting for a root Docker deploy daemon. Add bearer auth, mTLS, or bind loopback by default.

5. **Medium: daemon does expensive GitLab/download/decrypt work while holding the state mutex.**  
   `src/daemon.rs:311-316` calls `expected_hash_for_git_hash` under lock; `compare_cache` is also unbounded at `418-423`. Easy DoS. Compute outside the lock and bound the cache.

6. **Medium: TLS intent is inconsistent.**  
   `serve()` supports HTTP, logs `tls=disabled`, but CLI daemon mode requires `tlsCrt`/`tlsKey` at `src/cli.rs:459-464`. Pick one: mandatory TLS or genuinely optional TLS.

7. **Medium: suggest-image no-op path can create a bogus MR.**  
   If target branch already has the rendered content, `src/suggest.rs:189-209` skips commit, then may try creating an MR from a branch that was never created. Return “already up to date” unless an existing MR needs updating.

8. **Medium: systemd docs say unit is `0644`, but global umask makes first write `0600`.**  
   `main()` sets umask `077`; `src/systemd.rs:23` writes directly. Either `set_permissions(0644)` or update docs.

9. **Low but annoying: YAML updates lose comments/formatting.**  
   `serde_yaml` rewrites the whole manifest. For GitLab MRs, a targeted line edit would be much nicer.

Clippy failures are mostly hygiene: `is_some()`, `next_back()`, needless borrows, and too-many-args. Fix them, but the real blockers are #1–#4.
