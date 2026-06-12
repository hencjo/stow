use crate::app_error::AppError;
use crate::cli::{AssignAttempt, SuggestOptions};
use crate::command::capture_command;
use crate::gitlab::{
    GitLabClient, GitLabCommitAction, GitLabCommitRequest, GitLabConfig, MergeRequestPayload,
    MergeRequestResult, MergeRequestUpdatePayload,
};
use crate::manifest::{
    current_manifest_daemon_base_url, current_manifest_image_reference,
    ensure_version_not_downgraded, extract_digest, extract_tag, normalize_digest,
    update_manifest_image_reference, validate_tagged_image, STOW_DEFINITION_FILE,
};
use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

pub fn suggest(
    gitlab: &GitLabConfig,
    subfolder: &str,
    gitlab_token: &str,
    gitlab_auth_header: &str,
    opts: &SuggestOptions,
) -> Result<(), AppError> {
    println!("[suggest] Suggesting image bump to {}", opts.image);

    let client = GitLabClient::new(gitlab, gitlab_token, gitlab_auth_header);
    let assignee = resolve_assignee(&client, &opts.assign)?;
    let project = client.project_info()?;
    let target_branch = project.default_branch;
    let _ = client.branch_info(&target_branch)?.ok_or_else(|| {
        AppError::msg(format!("Target branch {target_branch} not found in GitLab"))
    })?;
    let service_path = service_file_path(subfolder);
    println!("[suggest] Fetching {service_path} from {target_branch} via GitLab Files API...");
    let content = client
        .file_contents(&service_path, &target_branch)?
        .ok_or_else(|| AppError::msg(format!("{service_path} not found in {target_branch}")))?;

    validate_tagged_image(&opts.image)?;
    let (selected_container, current_ref) =
        current_manifest_image_reference(&content, opts.container.as_deref())?;
    let daemon_base_url = current_manifest_daemon_base_url(&content);

    let current_tag = extract_tag(&current_ref)?;
    let new_tag = extract_tag(&opts.image)?;
    ensure_version_not_downgraded(&current_tag, &new_tag)?;

    let digest = if let Some(ref d) = opts.digest {
        normalize_digest(d).ok_or_else(|| {
            AppError::msg("--digest must be 64 hex characters, optionally prefixed with sha256:")
        })?
    } else {
        pull_and_get_digest(&opts.image)?
    };

    let new_ref = format!("{}@sha256:{}", opts.image, digest);
    let update = update_manifest_image_reference(&content, opts.container.as_deref(), &new_ref)?;

    let current_digest = extract_digest(&current_ref).unwrap_or_else(|| "unknown".to_string());
    let new_digest = extract_digest(&new_ref).unwrap_or_else(|| "unknown".to_string());

    println!("[suggest] Target container: {selected_container}");
    println!("[suggest] Current image: {current_ref}");
    println!("[suggest] Proposed image: {new_ref}");

    let changelog_changes = if new_tag != current_tag {
        opts.changelog_file.as_deref().and_then(|path| {
            match collect_changelog_changes(Path::new(path), &current_tag, &new_tag) {
                Ok(Some(changes)) => {
                    println!("[suggest] Including changelog changes from {path}.");
                    Some(changes)
                }
                Ok(None) => {
                    println!(
                        "[suggest] No changelog additions found between {current_tag} and {new_tag} in {path}."
                    );
                    None
                }
                Err(err) => {
                    println!("[suggest] Skipping changelog changes: {err}");
                    None
                }
            }
        })
    } else {
        None
    };

    println!("[suggest] Creating or updating merge request...");
    let mr = submit_merge_request(
        gitlab,
        gitlab_token,
        gitlab_auth_header,
        subfolder,
        &selected_container,
        &current_tag,
        &new_ref,
        &new_tag,
        &current_digest,
        &new_digest,
        &update.rendered,
        assignee,
        changelog_changes,
        daemon_base_url.as_deref(),
    )?;
    println!(
        "[suggest] Merge request {}: {}",
        if mr.updated { "updated" } else { "created" },
        mr.url
    );

    Ok(())
}

fn resolve_assignee(
    client: &GitLabClient<'_>,
    attempts: &[AssignAttempt],
) -> Result<Option<u64>, AppError> {
    for attempt in attempts {
        match attempt {
            AssignAttempt::GitLabUserId => match env::var("GITLAB_USER_ID") {
                Ok(value) => match value.trim().parse::<u64>() {
                    Ok(id) => {
                        println!(
                            "[suggest] Assigning merge request to GitLab user ID {id} (from GITLAB_USER_ID)."
                        );
                        return Ok(Some(id));
                    }
                    Err(_) => {
                        println!(
                            "[suggest] GITLAB_USER_ID=\"{value}\" is not a valid integer; trying next --assign item."
                        );
                    }
                },
                Err(_) => {
                    println!("[suggest] GITLAB_USER_ID is not set; trying next --assign item.");
                }
            },
            AssignAttempt::Id(id) => {
                println!(
                    "[suggest] Assigning merge request to GitLab user ID {id} (from --assign)."
                );
                return Ok(Some(*id));
            }
            AssignAttempt::User(username) => match client.user_id_by_username(username)? {
                Some(id) => {
                    println!("[suggest] Assigning merge request to GitLab user {username} ({id}).");
                    return Ok(Some(id));
                }
                None => {
                    println!(
                        "[suggest] GitLab assignee user {username} was not found; trying next --assign item."
                    );
                }
            },
        }
    }
    Ok(None)
}

fn submit_merge_request(
    gitlab: &GitLabConfig,
    gitlab_token: &str,
    gitlab_auth_header: &str,
    subfolder: &str,
    container_name: &str,
    current_tag: &str,
    new_ref: &str,
    new_tag: &str,
    current_digest: &str,
    new_digest: &str,
    rendered_service: &str,
    assignee: Option<u64>,
    changelog_changes: Option<String>,
    daemon_base_url: Option<&str>,
) -> Result<MergeRequestResult, AppError> {
    let client = GitLabClient::new(gitlab, gitlab_token, gitlab_auth_header);
    let project = client.project_info()?;
    let target_branch = project.default_branch;
    let target_branch_info = client.branch_info(&target_branch)?.ok_or_else(|| {
        AppError::msg(format!("Target branch {target_branch} not found in GitLab"))
    })?;

    let branch_name = suggestion_branch_name(subfolder, container_name);
    let service_path = service_file_path(subfolder);
    let target_content = client.file_contents(&service_path, &target_branch)?;
    let badge_revision = if target_content
        .as_deref()
        .map(|content| content != rendered_service)
        .unwrap_or(true)
    {
        let commit_payload = GitLabCommitRequest {
            branch: branch_name.clone(),
            start_branch: Some(target_branch.clone()),
            force: Some(true),
            last_commit_id: None,
            commit_message: format!("Bump {subfolder}/{container_name} image to {new_ref}"),
            actions: vec![GitLabCommitAction {
                action: "update".to_string(),
                file_path: service_path.clone(),
                content: rendered_service.to_string(),
            }],
        };
        client.create_commit(&commit_payload)?.id
    } else {
        println!("[suggest] Target branch already has the requested content; skipping commit.");
        target_branch_info.commit.id
    };

    let digest_changed = current_digest != new_digest;
    let title = merge_request_title(
        subfolder,
        container_name,
        current_tag,
        new_tag,
        digest_changed,
    );
    let description = merge_request_description(
        subfolder,
        container_name,
        current_tag,
        new_tag,
        current_digest,
        new_digest,
        &badge_revision,
        changelog_changes.as_deref(),
        daemon_base_url,
    );

    if let Some(existing) = client.find_open_merge_request(&branch_name, &target_branch)? {
        let payload = MergeRequestUpdatePayload {
            title,
            description,
            assignee_id: assignee,
            remove_source_branch: true,
        };
        let updated = client.update_merge_request(existing.iid, &payload)?;
        Ok(MergeRequestResult {
            url: updated.web_url,
            updated: true,
        })
    } else {
        let payload = MergeRequestPayload {
            source_branch: branch_name,
            target_branch,
            title,
            description,
            assignee_id: assignee,
            remove_source_branch: true,
        };
        let created = client.create_merge_request(&payload)?;
        Ok(MergeRequestResult {
            url: created.web_url,
            updated: false,
        })
    }
}

fn suggestion_branch_name(subfolder: &str, container_name: &str) -> String {
    let sanitized = format!("{subfolder}-{container_name}")
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '-',
        })
        .collect::<String>();
    format!("suggest/{sanitized}")
}

fn service_file_path(subfolder: &str) -> String {
    format!("{subfolder}/{STOW_DEFINITION_FILE}")
}

fn merge_request_title(
    subfolder: &str,
    container_name: &str,
    current_tag: &str,
    new_tag: &str,
    digest_changed: bool,
) -> String {
    if current_tag != new_tag {
        format!("Bump {subfolder}/{container_name} to {new_tag}")
    } else if digest_changed {
        format!("Digest refresh for {subfolder}/{container_name} ({new_tag})")
    } else {
        format!("Update {subfolder}/{container_name} image")
    }
}

fn merge_request_description(
    subfolder: &str,
    container_name: &str,
    current_tag: &str,
    new_tag: &str,
    current_digest: &str,
    new_digest: &str,
    revision: &str,
    changelog_changes: Option<&str>,
    daemon_base_url: Option<&str>,
) -> String {
    let mut body = if current_tag != new_tag {
        format!(
            "Version update for `{subfolder}/{container_name}` from `{current_tag}` to `{new_tag}`."
        )
    } else if current_digest != new_digest {
        format!(
            "⚠️ Digest-only refresh for `{subfolder}/{container_name}` (tag `{current_tag}`).\n\n- Previous digest: `{current_digest}`\n- Proposed digest: `{new_digest}`\n\nBase revision: `{revision}`"
        )
    } else {
        format!(
            "Automated suggestion for `{subfolder}/{container_name}` (no detected change).\n\nBase revision: `{revision}`"
        )
    };
    if let Some(base) = daemon_base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let normalized = base.trim_end_matches('/');
        let badge_url = format!("{normalized}/gitlab.svg?git_hash={}", url_encode(revision));
        let status_url = format!("{normalized}/status?head_hash={}", url_encode(revision));
        body.push_str(&format!(
            "\n\n[![stow convergence]({badge_url})]({status_url})"
        ));
    }
    if let Some(changes) = changelog_changes {
        body.push_str("\n\nChangelog:\n```markdown\n");
        body.push_str(changes);
        body.push_str("\n```");
    }
    body
}

fn url_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn collect_changelog_changes(
    changelog_path: &Path,
    current_tag: &str,
    new_tag: &str,
) -> Result<Option<String>, AppError> {
    if current_tag == new_tag {
        return Ok(None);
    }
    let range = format!("{current_tag}..{new_tag}");
    let output = Command::new("git")
        .arg("diff")
        .arg("--no-ext-diff")
        .arg("--find-renames")
        .arg("--unified=0")
        .arg(&range)
        .arg("--")
        .arg(changelog_path)
        .output()
        .map_err(|err| AppError::msg(format!("failed to run git diff for changelog: {err}")))?;
    if !output.status.success() {
        return Err(AppError::msg(format!(
            "git diff failed for {} between {current_tag} and {new_tag}: {}",
            changelog_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(changelog_added_lines(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn changelog_added_lines(diff: &str) -> Option<String> {
    let mut added = Vec::new();
    for line in diff.lines() {
        if line.starts_with("+++") {
            continue;
        }
        if let Some(content) = line.strip_prefix('+') {
            added.push(content);
        }
    }
    let changes = added.join("\n").trim().to_string();
    if changes.is_empty() {
        None
    } else {
        Some(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        changelog_added_lines, docker_arch, merge_request_description, merge_request_title,
        service_file_path, suggestion_branch_name, url_encode,
    };

    #[test]
    fn changelog_added_lines_strips_patch_format() {
        let diff = "\
diff --git a/CHANGELOG.md b/CHANGELOG.md
index 1111111..2222222 100644
--- a/CHANGELOG.md
+++ b/CHANGELOG.md
@@ -0,0 +1,4 @@
+## 20260428.0
+
+- Added demo support
+- Fixed deploys
";

        assert_eq!(
            changelog_added_lines(diff).as_deref(),
            Some("## 20260428.0\n\n- Added demo support\n- Fixed deploys")
        );
    }

    #[test]
    fn changelog_added_lines_preserves_literal_plus_lines() {
        let diff = "\
+++ b/CHANGELOG.md
@@ -0,0 +1 @@
++ literal plus
";

        assert_eq!(
            changelog_added_lines(diff).as_deref(),
            Some("+ literal plus")
        );
    }

    #[test]
    fn manifest_digest_parser_prefers_descriptor_over_config_digest() {
        let output = r#"{
          "Descriptor": {
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
          },
          "SchemaV2Manifest": {
            "config": {
              "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }
          }
        }"#;

        assert_eq!(
            super::parse_digest_from_manifest_output(output).as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn manifest_digest_parser_rejects_config_only_digest() {
        let output = r#"{
          "config": {
            "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
          }
        }"#;

        assert_eq!(super::parse_digest_from_manifest_output(output), None);
    }

    #[test]
    fn changelog_added_lines_returns_none_for_empty_diff() {
        assert_eq!(changelog_added_lines(""), None);
        assert_eq!(changelog_added_lines("--- a/x\n+++ b/x\n"), None);
        // whitespace-only additions collapse to None
        assert_eq!(changelog_added_lines("+\n+  \n"), None);
    }

    #[test]
    fn suggestion_branch_name_sanitizes_special_characters() {
        assert_eq!(
            suggestion_branch_name("deploy-host.example.com", "api"),
            "suggest/deploy-host.example.com-api"
        );
        assert_eq!(suggestion_branch_name("a/b c", "x:y"), "suggest/a-b-c-x-y");
    }

    #[test]
    fn service_file_path_appends_stow_yaml() {
        assert_eq!(
            service_file_path("host.example.com"),
            "host.example.com/stow.yaml"
        );
    }

    #[test]
    fn merge_request_title_distinguishes_bump_refresh_and_noop() {
        assert_eq!(
            merge_request_title("host", "api", "1.0", "1.1", true),
            "Bump host/api to 1.1"
        );
        assert_eq!(
            merge_request_title("host", "api", "1.0", "1.0", true),
            "Digest refresh for host/api (1.0)"
        );
        assert_eq!(
            merge_request_title("host", "api", "1.0", "1.0", false),
            "Update host/api image"
        );
    }

    #[test]
    fn merge_request_description_for_version_update() {
        let body = merge_request_description(
            "host", "api", "1.0", "1.1", "aaa", "bbb", "rev123", None, None,
        );
        assert_eq!(body, "Version update for `host/api` from `1.0` to `1.1`.");
    }

    #[test]
    fn merge_request_description_for_digest_only_refresh() {
        let body = merge_request_description(
            "host", "api", "1.0", "1.0", "aaa", "bbb", "rev123", None, None,
        );
        assert!(body.starts_with("⚠️ Digest-only refresh for `host/api` (tag `1.0`)."));
        assert!(body.contains("- Previous digest: `aaa`"));
        assert!(body.contains("- Proposed digest: `bbb`"));
        assert!(body.contains("Base revision: `rev123`"));
    }

    #[test]
    fn merge_request_description_for_no_detected_change() {
        let body = merge_request_description(
            "host", "api", "1.0", "1.0", "aaa", "aaa", "rev123", None, None,
        );
        assert!(body.starts_with("Automated suggestion for `host/api` (no detected change)."));
    }

    #[test]
    fn merge_request_description_appends_badge_and_changelog() {
        let body = merge_request_description(
            "host",
            "api",
            "1.0",
            "1.1",
            "aaa",
            "bbb",
            "rev/123",
            Some("- fixed things"),
            Some("https://daemon.example:17403/"),
        );
        // trailing slash trimmed, revision percent-encoded
        assert!(body.contains(
            "[![stow convergence](https://daemon.example:17403/gitlab.svg?git_hash=rev%2F123)](https://daemon.example:17403/status?head_hash=rev%2F123)"
        ));
        assert!(body.ends_with("Changelog:\n```markdown\n- fixed things\n```"));
    }

    #[test]
    fn merge_request_description_ignores_blank_daemon_url() {
        let body = merge_request_description(
            "host",
            "api",
            "1.0",
            "1.1",
            "aaa",
            "bbb",
            "rev",
            None,
            Some("   "),
        );
        assert!(!body.contains("stow convergence"));
    }

    #[test]
    fn url_encoding_escapes_non_alphanumerics() {
        assert_eq!(url_encode("abc123"), "abc123");
        assert_eq!(url_encode("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn docker_arch_maps_rust_arch_names() {
        assert_eq!(docker_arch("x86_64"), "amd64");
        assert_eq!(docker_arch("aarch64"), "arm64");
        assert_eq!(docker_arch("armv7"), "arm");
        assert_eq!(docker_arch("riscv64"), "riscv64");
    }

    #[test]
    fn manifest_digest_parser_selects_host_platform_from_manifest_list() {
        let host_arch = docker_arch(std::env::consts::ARCH);
        let host_os = std::env::consts::OS;
        let output = format!(
            r#"{{
              "manifests": [
                {{
                  "digest": "sha256:{other}",
                  "platform": {{ "os": "{host_os}", "architecture": "not-{host_arch}" }}
                }},
                {{
                  "digest": "sha256:{matching}",
                  "platform": {{ "os": "{host_os}", "architecture": "{host_arch}" }}
                }}
              ]
            }}"#,
            other = "c".repeat(64),
            matching = "d".repeat(64),
        );
        assert_eq!(
            super::parse_digest_from_manifest_output(&output).as_deref(),
            Some("d".repeat(64).as_str())
        );
    }

    #[test]
    fn manifest_digest_parser_falls_back_to_first_entry_without_platform_match() {
        let output = format!(
            r#"{{
              "manifests": [
                {{ "digest": "sha256:{first}", "platform": {{ "os": "plan9", "architecture": "mips" }} }},
                {{ "digest": "sha256:{second}", "platform": {{ "os": "plan9", "architecture": "mips" }} }}
              ]
            }}"#,
            first = "e".repeat(64),
            second = "f".repeat(64),
        );
        assert_eq!(
            super::parse_digest_from_manifest_output(&output).as_deref(),
            Some("e".repeat(64).as_str())
        );
    }

    #[test]
    fn manifest_digest_parser_handles_plain_digest_field_and_garbage() {
        let output = format!(r#"{{ "Digest": "sha256:{}" }}"#, "a".repeat(64));
        assert_eq!(
            super::parse_digest_from_manifest_output(&output).as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(super::parse_digest_from_manifest_output("not json"), None);
        assert_eq!(super::parse_digest_from_manifest_output("{}"), None);
    }
}

fn pull_and_get_digest(image: &str) -> Result<String, AppError> {
    match fetch_digest_via_manifest(image) {
        Ok(digest) => return Ok(digest),
        Err(err) => {
            println!(
                "[suggest] docker manifest inspect failed to provide digest: {err}. Falling back to docker pull."
            );
        }
    }
    pull_image_and_get_local_digest(image)
}

fn fetch_digest_via_manifest(image: &str) -> Result<String, AppError> {
    println!("[suggest] Querying registry for digest via docker manifest inspect...");
    let attempts: [&[&OsStr]; 2] = [
        &[
            OsStr::new("manifest"),
            OsStr::new("inspect"),
            OsStr::new("--verbose"),
            OsStr::new(image),
        ],
        &[
            OsStr::new("manifest"),
            OsStr::new("inspect"),
            OsStr::new(image),
        ],
    ];
    let mut last_err: Option<AppError> = None;
    for args in attempts {
        match capture_command("docker", args) {
            Ok(output) => {
                if let Some(digest) = parse_digest_from_manifest_output(&output) {
                    return Ok(digest);
                }
                last_err = Some(AppError::msg(
                    "docker manifest inspect output did not contain a digest",
                ));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err
        .unwrap_or_else(|| AppError::msg("Unable to obtain digest via docker manifest inspect")))
}

fn parse_digest_from_manifest_output(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    descriptor_digest(&value)
        .or_else(|| platform_manifest_digest(&value))
        .and_then(normalize_digest)
}

fn platform_manifest_digest(value: &Value) -> Option<&str> {
    let manifests = manifest_entries(value)?;
    let platform = host_platform();
    let mut fallback = None;
    for entry in manifests {
        let digest = descriptor_digest(entry);
        if digest.is_none() {
            continue;
        }
        if entry
            .get("platform")
            .map(|p| platform_matches(p, &platform))
            .unwrap_or(false)
        {
            return digest;
        }
        if fallback.is_none() {
            fallback = digest;
        }
    }
    fallback
}

fn descriptor_digest(value: &Value) -> Option<&str> {
    value
        .get("Descriptor")
        .and_then(|v| v.get("digest"))
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("descriptor")
                .and_then(|v| v.get("digest"))
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("Digest").and_then(Value::as_str))
        .or_else(|| value.get("digest").and_then(Value::as_str))
}

fn manifest_entries(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("SchemaV2ManifestList")
        .or_else(|| value.get("schemaV2ManifestList"))
        .and_then(|v| v.get("manifests"))
        .and_then(Value::as_array)
        .or_else(|| value.get("manifests").and_then(Value::as_array))
}

struct HostPlatform {
    os: &'static str,
    arch: String,
}

fn host_platform() -> HostPlatform {
    HostPlatform {
        os: std::env::consts::OS,
        arch: docker_arch(std::env::consts::ARCH),
    }
}

fn docker_arch(arch: &str) -> String {
    match arch {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        "arm" => "arm".to_string(),
        "armv7" | "armv7l" => "arm".to_string(),
        "s390x" => "s390x".to_string(),
        "ppc64" | "powerpc64" => "ppc64".to_string(),
        "ppc64le" => "ppc64le".to_string(),
        "mips64" => "mips64".to_string(),
        "mips64el" => "mips64le".to_string(),
        other => other.to_string(),
    }
}

fn platform_matches(value: &Value, platform: &HostPlatform) -> bool {
    let os_match = value
        .get("os")
        .and_then(Value::as_str)
        .map(|os| os.eq_ignore_ascii_case(platform.os))
        .unwrap_or(false);
    let arch_match = value
        .get("architecture")
        .and_then(Value::as_str)
        .map(|arch| arch.eq_ignore_ascii_case(&platform.arch))
        .unwrap_or(false);
    os_match && arch_match
}

fn pull_image_and_get_local_digest(image: &str) -> Result<String, AppError> {
    println!("[suggest] Pulling image {} to get digest...", image);
    let status = Command::new("docker").arg("pull").arg(image).status()?;
    if !status.success() {
        return Err(AppError::msg(format!("docker pull failed for {}", image)));
    }

    let output = capture_command(
        "docker",
        &[
            OsStr::new("inspect"),
            OsStr::new("--format"),
            OsStr::new("{{index .RepoDigests 0}}"),
            OsStr::new(image),
        ],
    )?;

    let Some((_, digest)) = output.trim().rsplit_once("@sha256:") else {
        return Err(AppError::msg(format!(
            "Failed to extract digest from docker inspect output: {}",
            output
        )));
    };

    normalize_digest(digest).ok_or_else(|| {
        AppError::msg(format!(
            "Invalid digest format from docker inspect: {}",
            digest
        ))
    })
}
