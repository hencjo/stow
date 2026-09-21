//! End-to-end CLI contract, using an isolated GitLab and no real registry/host.
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

const PROJECT: &str = "/api/v4/projects/team/deployments";
const BRANCH: &str = "suggest/hosts-kappa-kappa";
const FILE: &str = "hosts/kappa/stow.yaml";
const PRIVATE: &str = "private-token-not-for-logs";
const JOB: &str = "job-token-not-for-logs";

fn digest(ch: char) -> String {
    ch.to_string().repeat(64)
}
fn manifest(tag: &str) -> String {
    format!("# deployed 0.2 shape\ndeployment:\n  name: kappa\n  daemonBaseUrl: https://daemon.example/\ncontainers:\n  - name: kappa\n    image: registry.example/kappa:{tag}@sha256:{}\n    env:\n      OTEL_SERVICE_NAME: \"kappa\" # keep\n      RELEASE_VERSION: \"old\"\n", digest('a'))
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct State {
    target: String,
    head: String,
    head_reads: usize,
    move_target: bool,
    pending: Option<(String, String)>,
    mr: Option<Value>,
    fail_mr_once: bool,
    deny_commit: bool,
    requests: Vec<Request>,
    commits: Vec<Value>,
    unexpected: Vec<String>,
}

impl State {
    fn respond(&mut self, r: &Request) -> (u16, String) {
        let suffix = r.path.strip_prefix(PROJECT).unwrap_or(&r.path);
        if r.method == "GET" {
            if suffix.is_empty() {
                return (200, json!({"default_branch": "production"}).to_string());
            }
            if suffix == "/repository/branches/production" {
                self.head_reads += 1;
                if self.move_target && self.head_reads == 2 {
                    self.head = "moved".into();
                }
                return (200, json!({"commit": {"id": self.head}}).to_string());
            }
            if suffix == format!("/repository/branches/{BRANCH}") {
                return match &self.pending {
                    Some((sha, _)) => (200, json!({"commit": {"id": sha}}).to_string()),
                    None => (404, "{}".into()),
                };
            }
            if let Some(reference) =
                suffix.strip_prefix(&format!("/repository/files/{FILE}/raw?ref="))
            {
                if reference == self.head {
                    return (200, self.target.clone());
                }
                if let Some((sha, content)) = &self.pending {
                    if sha == reference {
                        return (200, content.clone());
                    }
                }
                self.unexpected
                    .push(format!("non-immutable/unknown ref {reference}"));
                return (404, "{}".into());
            }
            if suffix
                == format!(
                    "/merge_requests?state=opened&source_branch={BRANCH}&target_branch=production"
                )
            {
                return (
                    200,
                    if self.mr.is_some() {
                        json!([{"iid": 7, "web_url": "https://gitlab/mr/7"}])
                    } else {
                        json!([])
                    }
                    .to_string(),
                );
            }
            if r.path.starts_with("/api/v4/users?username=") {
                return (
                    200,
                    if r.path.ends_with("henrsjos") {
                        json!([{"id": 42, "username": "henrsjos"}])
                    } else {
                        json!([])
                    }
                    .to_string(),
                );
            }
        }
        if r.method == "POST" && suffix == "/repository/commits" {
            if self.deny_commit {
                return (403, format!("{PRIVATE} value-that-must-not-leak"));
            }
            self.commits.push(r.body.clone());
            let sha = format!("suggestion/{}", self.commits.len());
            self.pending = Some((
                sha.clone(),
                r.body["actions"][0]["content"].as_str().unwrap().into(),
            ));
            return (201, json!({"id": sha}).to_string());
        }
        if (r.method == "POST" && suffix == "/merge_requests")
            || (r.method == "PUT" && suffix == "/merge_requests/7")
        {
            if self.fail_mr_once {
                self.fail_mr_once = false;
                return (500, "MR failed".into());
            }
            self.mr = Some(r.body.clone());
            return (
                200,
                json!({"iid": 7, "web_url": "https://gitlab/mr/7"}).to_string(),
            );
        }
        self.unexpected.push(format!("{} {}", r.method, r.path));
        (500, "unexpected request".into())
    }
}

struct GitLab {
    base: String,
    dir: PathBuf,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl GitLab {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "stow-suggest-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("bin")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/api/v4", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            target: manifest(tag),
            head: "base".into(),
            head_reads: 0,
            move_target: false,
            pending: None,
            mr: None,
            fail_mr_once: false,
            deny_commit: false,
            requests: vec![],
            commits: vec![],
            unexpected: vec![],
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let s = state.clone();
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        let mut state = s.lock().unwrap();
                        let (status, body) = state.respond(&request);
                        state.requests.push(request);
                        write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock listener: {e}"),
                }
            }
        });
        Self {
            base,
            dir,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn command(&self, tag: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stow"));
        cmd.env_clear()
            .current_dir(&self.dir)
            .env("HOME", &self.dir)
            .env("HOSTNAME", "ci")
            // Empty bin: provided digests must not invoke Git or Docker.
            .env("PATH", self.dir.join("bin"))
            .env("CI_API_V4_URL", &self.base)
            .env("GITLAB_ACCESS_TOKEN", format!("  {PRIVATE} \n"))
            .env("CI_JOB_TOKEN", format!(" {JOB} "))
            .args([
                "suggest-image",
                "--project",
                "team/deployments",
                "--subfolder",
                "hosts/kappa",
                "--container",
                "kappa",
                "--image",
                &format!("registry.example/kappa:{tag}"),
            ]);
        cmd
    }

    fn run(&self, tag: &str, digest_char: char, args: &[&str]) -> Output {
        self.command(tag)
            .args(["--digest", &digest(digest_char)])
            .args(args)
            .output()
            .unwrap()
    }

    fn writes(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.method != "GET")
            .count()
    }
}

impl Drop for GitLab {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().into();
    let path = percent_decode_str(parts.next().unwrap())
        .decode_utf8()
        .unwrap()
        .into_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        headers,
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn failure(output: Output) -> String {
    assert!(!output.status.success());
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn old_ci_pins_image_preserves_badge_and_prefers_private_token() {
    let gitlab = GitLab::new("1.0");
    let out = success(
        gitlab
            .command("1.1")
            .args([
                "--digest",
                &format!("sha256:{}", digest('B')),
                "--assign",
                "gitlab_user_id,user:missing,user:henrsjos",
            ])
            .output()
            .unwrap(),
    );
    assert!(out.contains("created: https://gitlab/mr/7"));
    assert!(!out.contains(PRIVATE));
    let s = gitlab.state.lock().unwrap();
    assert!(s.unexpected.is_empty(), "{:?}", s.unexpected);
    assert!(s
        .requests
        .iter()
        .all(
            |r| r.headers.get("private-token").map(String::as_str) == Some(PRIVATE)
                && !r.headers.contains_key("job-token")
        ));
    assert_eq!(s.commits.len(), 1);
    let c = &s.commits[0];
    assert_eq!(c["start_sha"], "base");
    assert!(c.get("start_branch").is_none());
    assert_eq!(c["force"], true);
    assert_eq!(c["branch"], BRANCH);
    assert_eq!(c["actions"].as_array().unwrap().len(), 1);
    assert_eq!(c["actions"][0]["action"], "update");
    assert_eq!(c["actions"][0]["file_path"], FILE);
    assert!(c["actions"][0]["content"]
        .as_str()
        .unwrap()
        .contains(&format!(":1.1@sha256:{}", digest('b'))));
    let mr = s.mr.as_ref().unwrap();
    assert_eq!(mr["title"], "Bump hosts/kappa/kappa to 1.1");
    assert_eq!(mr["assignee_id"], 42);
    assert_eq!(mr["target_branch"], "production");
    assert_eq!(mr["remove_source_branch"], true);
    assert!(mr["description"].as_str().unwrap().contains("[![stow convergence](https://daemon.example/gitlab.svg?git_hash=suggestion%2F1)](https://daemon.example/status?head_hash=suggestion%2F1)"));
}

#[test]
fn release_retry_reuses_commit_and_mr_and_after_merge_is_noop() {
    let gitlab = GitLab::new("old-revision");
    gitlab.state.lock().unwrap().fail_mr_once = true;
    fs::create_dir(gitlab.dir.join("kappa")).unwrap();
    fs::write(
        gitlab.dir.join("kappa/CHANGELOG.md"),
        "\n- New release\n## Fixes\n~~~~rust\n# not a release\n~~~~\n# Kappa older\n- old",
    )
    .unwrap();
    let args = [
        "--release-version",
        "20260914.0",
        "--env",
        "RELEASE_VERSION=20260914.0",
        "--changelog-file",
        "kappa/CHANGELOG.md",
    ];
    assert!(failure(gitlab.run("370d336", 'b', &args)).contains("HTTP 500"));
    assert_eq!(gitlab.state.lock().unwrap().commits.len(), 1);
    success(gitlab.run("370d336", 'b', &args));
    let out = success(gitlab.run("370d336", 'b', &args));
    assert!(out.contains("updated:"));
    {
        let mut s = gitlab.state.lock().unwrap();
        assert!(s.unexpected.is_empty(), "{:?}", s.unexpected);
        assert_eq!(s.commits.len(), 1);
        let content = &s.pending.as_ref().unwrap().1;
        assert!(content.contains(&format!(":370d336@sha256:{}", digest('b'))));
        assert!(content.contains("RELEASE_VERSION: \"20260914.0\""));
        assert!(content.contains("OTEL_SERVICE_NAME: \"kappa\" # keep"));
        assert!(!content.contains("SOURCE_REVISION"));
        let mr = s.mr.as_ref().unwrap();
        assert_eq!(mr["title"], "Release hosts/kappa/kappa 20260914.0");
        let body = mr["description"].as_str().unwrap();
        assert!(body.contains("RELEASE_VERSION: old -> 20260914.0"));
        assert!(body.contains("Changelog (20260914.0):"));
        assert!(body.contains("## Fixes\n~~~~rust\n# not a release\n~~~~"));
        assert!(!body.contains("# Kappa older"));
        assert!(body.find("Environment changes:").unwrap() < body.find("Changelog (").unwrap());
        assert!(body.contains("gitlab.svg?git_hash=suggestion%2F1"));
        // Simulate merge and source-branch deletion.
        s.target = s.pending.take().unwrap().1;
        s.head = "merged".into();
        s.mr = None;
    }
    let before = gitlab.writes();
    success(gitlab.run("370d336", 'b', &args));
    assert_eq!(gitlab.writes(), before);
    assert!(gitlab.state.lock().unwrap().mr.is_none());
}

#[test]
fn latest_suggestion_replaces_pending_changes_from_target_not_previous_proposal() {
    let gitlab = GitLab::new("1");
    success(gitlab.run(
        "1",
        'a',
        &["--env", "A=pending", "--env", "RELEASE_VERSION=pending"],
    ));
    success(gitlab.run("1", 'a', &["--env", "RELEASE_VERSION=latest"]));
    let s = gitlab.state.lock().unwrap();
    assert_eq!(s.commits.len(), 2);
    assert!(s
        .commits
        .iter()
        .all(|c| c["start_sha"] == "base" && c["branch"] == BRANCH));
    let rendered = &s.pending.as_ref().unwrap().1;
    assert!(!rendered.contains("A: "));
    let mr = s.mr.as_ref().unwrap();
    assert_eq!(mr["title"], "Update hosts/kappa/kappa environment");
    let body = mr["description"].as_str().unwrap();
    assert!(body.starts_with("Environment update"));
    assert!(body.contains("RELEASE_VERSION: old -> latest"));
    assert!(body.contains("gitlab.svg?git_hash=suggestion%2F2"));
}

#[test]
fn noops_skip_changelog_assignee_and_stale_mr_work() {
    let gitlab = GitLab::new("1");
    gitlab.state.lock().unwrap().mr = Some(json!({"title": "stale"}));
    let out = success(gitlab.run(
        "1",
        'a',
        &[
            "--release-version",
            "metadata-only",
            "--env",
            "RELEASE_VERSION=temporary",
            "--env",
            "RELEASE_VERSION=old",
            "--changelog-file",
            "missing",
            "--assign",
            "user:henrsjos",
        ],
    ));
    assert!(out.contains("nothing to do"));
    assert!(!out.contains("changelog"));
    let s = gitlab.state.lock().unwrap();
    assert_eq!(s.requests.len(), 3);
    assert_eq!(s.mr.as_ref().unwrap()["title"], "stale");
    assert!(s.commits.is_empty());
}

#[test]
fn target_movement_and_permission_failure_never_report_success() {
    let gitlab = GitLab::new("1");
    gitlab.state.lock().unwrap().move_target = true;
    assert!(failure(gitlab.run("2", 'b', &[]))
        .contains("target branch changed while preparing suggestion; retry"));
    assert_eq!(gitlab.writes(), 0);
    let gitlab = GitLab::new("1");
    gitlab.state.lock().unwrap().deny_commit = true;
    let err = failure(gitlab.run("2", 'b', &["--env", "A=value-that-must-not-leak"]));
    assert!(err.contains("HTTP 403"));
    assert!(!err.contains(PRIVATE));
    assert!(!err.contains("value-that-must-not-leak"));
    assert!(gitlab.state.lock().unwrap().mr.is_none());
}

#[test]
fn token_and_assignee_fallbacks_are_ordered_and_trimmed() {
    for (user_id, assign, expected) in [
        ("91", "gitlab_user_id,user:henrsjos", 91),
        ("invalid", "gitlab_user_id,id:17,user:henrsjos", 17),
        ("", "gitlab_user_id,user:missing,user:henrsjos", 42),
    ] {
        let gitlab = GitLab::new("1");
        success(
            gitlab
                .command("2")
                .env("GITLAB_ACCESS_TOKEN", " \t")
                .env("GITLAB_USER_ID", user_id)
                .args(["--digest", &digest('b'), "--assign", assign])
                .output()
                .unwrap(),
        );
        let s = gitlab.state.lock().unwrap();
        assert!(s
            .requests
            .iter()
            .all(
                |r| r.headers.get("job-token").map(String::as_str) == Some(JOB)
                    && !r.headers.contains_key("private-token")
            ));
        assert_eq!(s.mr.as_ref().unwrap()["assignee_id"], expected);
        if expected != 42 {
            assert!(!s.requests.iter().any(|r| r.path.contains("/users?")));
        }
    }
}

#[test]
fn downgrade_guard_and_opaque_tag_bypass_are_compatible() {
    for (old, new) in [
        ("2.0", "1.0"),
        ("1.0-2-gabc", "1.0"),
        ("old-rev", "370d336"),
    ] {
        let gitlab = GitLab::new(old);
        failure(gitlab.run(new, 'b', &["--env", "A=1"]));
        assert_eq!(gitlab.writes(), 0);
        success(gitlab.run(new, 'b', &["--release-version", "release", "--env", "A=1"]));
    }
    let gitlab = GitLab::new("1.0-2-gabc");
    success(gitlab.run("1.1", 'b', &[]));
}

#[test]
fn literal_values_and_repeated_keys_survive_the_actual_cli() {
    let gitlab = GitLab::new("1");
    let literal = "  Å字\n$HOME {{literal}}=true  ";
    let out = success(gitlab.run(
        "1",
        'a',
        &[
            "--env",
            "B=intermediate",
            "--env",
            "A=",
            "--env",
            &format!("B={literal}"),
            "--env",
            "RELEASE_VERSION=old",
            "--release-version",
            "ignored",
            "--release-version",
            "opaque [version]",
        ],
    ));
    assert!(!out.contains(literal));
    let s = gitlab.state.lock().unwrap();
    let yaml: serde_yaml::Value = serde_yaml::from_str(&s.pending.as_ref().unwrap().1).unwrap();
    let env = &yaml["containers"][0]["env"];
    assert_eq!(env["B"].as_str(), Some(literal));
    assert_eq!(env["A"].as_str(), Some(""));
    let mr = s.mr.as_ref().unwrap();
    assert_eq!(
        mr["title"],
        "Release hosts/kappa/kappa opaque \\[version\\]"
    );
    let body = mr["description"].as_str().unwrap();
    assert!(body.find("- B:").unwrap() < body.find("- A:").unwrap());
    assert!(body.contains("- A: unset -> (empty)"));
    assert!(!body.contains("- RELEASE_VERSION:"));
    assert!(body.contains("\\n$HOME \\{\\{literal\\}\\}=true"));
}

#[test]
fn invalid_cli_arguments_fail_before_gitlab() {
    let gitlab = GitLab::new("1");
    for args in [
        vec!["--env"],
        vec!["--env", "--release-version", "1"],
        vec!["--env", "--help"],
        vec!["--release-version"],
        vec!["--release-version", "--version"],
        vec!["--env", "A"],
        vec!["--env", "=x"],
        vec!["--env", "1A=x"],
        vec!["--env", "A-B=x"],
        vec!["--release-version", " \t"],
        vec!["--release-version", "bad\nversion"],
        vec!["--release-version", "bad\u{7f}version"],
        vec!["--config", "ignored"],
    ] {
        failure(gitlab.run("2", 'b', &args));
    }
    assert!(gitlab.state.lock().unwrap().requests.is_empty());
    for mode in ["reconcile", "daemon", "install-systemd"] {
        fs::write(gitlab.dir.join("config.yaml"), "{}\n").unwrap();
        for flag in ["--env", "--release-version"] {
            let out = Command::new(env!("CARGO_BIN_EXE_stow"))
                .env("HOME", &gitlab.dir)
                .env("HOSTNAME", "test")
                .args([
                    mode,
                    "--config",
                    gitlab.dir.join("config.yaml").to_str().unwrap(),
                    flag,
                    "A=x",
                ])
                .output()
                .unwrap();
            assert!(failure(out).contains("only available in suggest-image mode"));
        }
    }
}

#[test]
fn changelog_errors_are_nonfatal_in_both_modes() {
    for args in [
        vec!["--changelog-file", "missing"],
        vec!["--release-version", "new", "--changelog-file", "missing"],
        vec!["--release-version", "new", "--changelog-file", "mismatch"],
        vec!["--release-version", "new", "--changelog-file", "empty"],
    ] {
        let gitlab = GitLab::new("1");
        fs::write(
            gitlab.dir.join("mismatch"),
            "# Kappa old\n- wrong\n# Kappa new\n- do not search",
        )
        .unwrap();
        fs::write(gitlab.dir.join("empty"), "\n").unwrap();
        success(gitlab.run("2", 'b', &args));
        assert!(
            !gitlab.state.lock().unwrap().mr.as_ref().unwrap()["description"]
                .as_str()
                .unwrap()
                .contains("Changelog")
        );
    }
}

#[test]
fn omitted_digest_retains_docker_manifest_then_pull_inspect_fallback() {
    use std::os::unix::fs::PermissionsExt;
    let gitlab = GitLab::new("1");
    let script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> docker-calls\ncase \"$1\" in\nmanifest) exit 1;;\npull) exit 0;;\ninspect) printf 'registry.example/kappa:2@sha256:{}\\n';;\n*) exit 99;;\nesac\n", digest('b'));
    let path = gitlab.dir.join("bin/docker");
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    success(gitlab.command("2").output().unwrap());
    let calls = fs::read_to_string(gitlab.dir.join("docker-calls")).unwrap();
    assert!(calls.contains("manifest inspect --verbose registry.example/kappa:2"));
    assert!(calls.contains("pull registry.example/kappa:2"));
    assert!(calls.contains("inspect --format {{index .RepoDigests 0}} registry.example/kappa:2"));
    assert!(gitlab
        .state
        .lock()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .1
        .contains(&digest('b')));
}

#[test]
fn legacy_changelog_uses_local_git_tags_and_missing_refs_are_nonfatal() {
    let gitlab = GitLab::new("1");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&gitlab.dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    fs::create_dir(gitlab.dir.join("kappa")).unwrap();
    let changelog = gitlab.dir.join("kappa/CHANGELOG.md");
    fs::write(&changelog, "# Previous\n- old\n").unwrap();
    git(&["add", "kappa/CHANGELOG.md"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "old",
    ]);
    git(&["tag", "1"]);
    fs::write(&changelog, "- new local entry\n\n# Previous\n- old\n").unwrap();
    git(&["add", "kappa/CHANGELOG.md"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "new",
    ]);
    git(&["tag", "2"]);
    for tag in ["2", "3"] {
        success(
            gitlab
                .command(tag)
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args([
                    "--digest",
                    &digest('b'),
                    "--changelog-file",
                    "kappa/CHANGELOG.md",
                ])
                .output()
                .unwrap(),
        );
        let s = gitlab.state.lock().unwrap();
        let body = s.mr.as_ref().unwrap()["description"].as_str().unwrap();
        assert_eq!(
            body.contains("Changelog:\n```markdown\n- new local entry\n```"),
            tag == "2"
        );
    }
}

#[test]
fn same_tag_env_release_collects_local_changelog_but_legacy_mode_skips_it() {
    let gitlab = GitLab::new("1");
    fs::write(
        gitlab.dir.join("CHANGELOG.md"),
        "# Kappa next\n- same image release\n",
    )
    .unwrap();
    success(gitlab.run(
        "1",
        'a',
        &["--env", "A=one", "--changelog-file", "CHANGELOG.md"],
    ));
    assert!(
        !gitlab.state.lock().unwrap().mr.as_ref().unwrap()["description"]
            .as_str()
            .unwrap()
            .contains("Changelog:")
    );
    success(gitlab.run(
        "1",
        'a',
        &[
            "--env",
            "A=one",
            "--release-version",
            "next",
            "--changelog-file",
            "CHANGELOG.md",
        ],
    ));
    let s = gitlab.state.lock().unwrap();
    assert_eq!(s.commits.len(), 1); // only metadata changed; reuse pending commit
    assert!(s.mr.as_ref().unwrap()["description"]
        .as_str()
        .unwrap()
        .contains("Changelog (next):\n```markdown\n# Kappa next\n- same image release\n```"));
}

#[test]
fn release_version_does_not_disable_image_or_digest_validation() {
    let gitlab = GitLab::new("1");
    for image in ["repo-without-tag", "repo:tag@sha256:abc", "repo: "] {
        failure(
            gitlab
                .command("2")
                .args([
                    "--image",
                    image,
                    "--digest",
                    &digest('b'),
                    "--release-version",
                    "test",
                ])
                .output()
                .unwrap(),
        );
    }
    for digest in ["abc", "sha256:", &"g".repeat(64), &"a".repeat(65)] {
        failure(
            gitlab
                .command("2")
                .args(["--digest", digest, "--release-version", "test"])
                .output()
                .unwrap(),
        );
    }
    assert_eq!(gitlab.writes(), 0);
}
