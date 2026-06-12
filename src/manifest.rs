use crate::app_error::AppError;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

pub const STOW_DEFINITION_FILE: &str = "stow.yaml";

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentManifest {
    pub deployment_name: String,
    pub daemon_base_url: Option<String>,
    pub containers: Vec<DesiredContainerSpec>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesiredContainerSpec {
    pub name: String,
    pub image: String,
    pub docker_flags: Vec<String>,
    pub app_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MultiContainerFile {
    deployment: DeploymentSection,
    containers: Vec<MultiContainerSpec>,
}

#[derive(Debug, Deserialize)]
struct DeploymentSection {
    name: String,
    #[serde(rename = "daemonBaseUrl", default)]
    daemon_base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiContainerSpec {
    name: String,
    image: String,
    #[serde(rename = "restartPolicy")]
    restart_policy: Option<String>,
    memory: Option<String>,
    #[serde(default)]
    publish: Vec<String>,
    #[serde(default)]
    volumes: Vec<VolumeMount>,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct VolumeMount {
    source: String,
    target: String,
    #[serde(rename = "readOnly", default)]
    read_only: bool,
    #[serde(default)]
    selinux: Option<String>,
}

pub fn load_manifest(
    dir: &Path,
    runtime_dir: Option<&Path>,
) -> Result<DeploymentManifest, AppError> {
    let spec_path = dir.join(STOW_DEFINITION_FILE);
    if !spec_path.exists() {
        return Err(AppError::msg(format!(
            "Service definition missing: {}",
            spec_path.display()
        )));
    }
    let raw = std::fs::read_to_string(&spec_path)
        .map_err(|err| AppError::msg(format!("Failed to read {}: {err}", spec_path.display())))?;
    build_manifest(dir, runtime_dir, &raw, &spec_path)
}

fn build_manifest(
    config_root: &Path,
    runtime_dir: Option<&Path>,
    raw: &str,
    spec_path: &Path,
) -> Result<DeploymentManifest, AppError> {
    let definition: MultiContainerFile = serde_yaml::from_str(raw).map_err(|err| {
        AppError::msg(format!(
            "Failed to parse {} as multi-container manifest: {err}",
            spec_path.display()
        ))
    })?;
    build_multi_manifest(config_root, runtime_dir, definition)
}

fn build_multi_manifest(
    config_root: &Path,
    runtime_dir: Option<&Path>,
    definition: MultiContainerFile,
) -> Result<DeploymentManifest, AppError> {
    let deployment_name = definition.deployment.name.trim().to_string();
    if deployment_name.is_empty() {
        return Err(AppError::msg("deployment.name must be non-empty"));
    }
    let daemon_base_url = definition
        .deployment
        .daemon_base_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if definition.containers.is_empty() {
        return Err(AppError::msg("containers must contain at least one item"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut containers = Vec::with_capacity(definition.containers.len());
    for item in definition.containers {
        let name = item.name.trim().to_string();
        if !seen.insert(name.clone()) {
            return Err(AppError::msg(format!(
                "containers contain duplicate name {name}"
            )));
        }
        containers.push(build_container_spec(
            config_root,
            runtime_dir,
            &name,
            &item.image,
            item.restart_policy.as_deref(),
            item.memory.as_deref(),
            &item.publish,
            &item.volumes,
            item.args,
        )?);
    }
    Ok(DeploymentManifest {
        deployment_name,
        daemon_base_url,
        containers,
    })
}

fn build_container_spec(
    config_root: &Path,
    runtime_dir: Option<&Path>,
    container_name: &str,
    image_ref: &str,
    restart_policy: Option<&str>,
    memory: Option<&str>,
    publish: &[String],
    volumes: &[VolumeMount],
    args: Vec<String>,
) -> Result<DesiredContainerSpec, AppError> {
    let name = container_name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::msg("container name must be non-empty"));
    }
    let image = image_ref.trim().to_string();
    validate_image_reference(&image)?;

    let mut docker_flags = Vec::new();
    if let Some(policy) = restart_policy.map(str::trim) {
        if !policy.is_empty() {
            docker_flags.push("--restart".to_string());
            docker_flags.push(policy.to_string());
        }
    }
    if let Some(memory) = memory.map(str::trim) {
        if !memory.is_empty() {
            docker_flags.push("--memory".to_string());
            docker_flags.push(memory.to_string());
        }
    }
    for publish in publish {
        let trimmed = publish.trim();
        if !trimmed.is_empty() {
            docker_flags.push("--publish".to_string());
            docker_flags.push(trimmed.to_string());
        }
    }
    for volume in volumes {
        let source_value = volume.source.trim();
        let resolved_source = resolve_volume_source(source_value, config_root, runtime_dir)?;
        let target = volume.target.trim();
        if target.is_empty() {
            return Err(AppError::msg(
                "container.volumes[].target must be non-empty",
            ));
        }
        let mut spec = format!("{}:{}", resolved_source.to_string_lossy(), target);
        let mut options: Vec<String> = Vec::new();
        if let Some(selinux) = volume.selinux.as_deref().map(str::trim) {
            if !selinux.is_empty() {
                options.push(selinux.to_string());
            }
        }
        if volume.read_only {
            options.push("ro".to_string());
        }
        if !options.is_empty() {
            spec.push(':');
            spec.push_str(&options.join(","));
        }
        docker_flags.push("--volume".to_string());
        docker_flags.push(spec);
    }

    let app_args = args
        .into_iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect();

    Ok(DesiredContainerSpec {
        name,
        image,
        docker_flags,
        app_args,
    })
}

pub fn validate_image_reference(image: &str) -> Result<(), AppError> {
    let Some((name_and_tag, digest_part)) = image.rsplit_once("@sha256:") else {
        return Err(AppError::msg(
            "image reference must include a tag and sha256 digest (name:tag@sha256:...)",
        ));
    };
    if image_tag(name_and_tag).is_none() {
        return Err(AppError::msg(
            "image reference must include an explicit tag before the digest (name:tag@sha256:...)",
        ));
    }
    if digest_part.len() != 64 || !digest_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::msg(format!(
            "image digest must be 64 hex characters (got {digest_part})"
        )));
    }
    Ok(())
}

pub fn validate_tagged_image(image: &str) -> Result<(), AppError> {
    if image.contains('@') {
        return Err(AppError::msg(
            "--image should not include a digest; provide name:tag and optionally supply --digest",
        ));
    }
    let Some(tag) = image_tag(image) else {
        return Err(AppError::msg(
            "--image must include an explicit tag (name:tag)",
        ));
    };
    if tag.trim().is_empty() {
        return Err(AppError::msg("--image tag must be non-empty"));
    }
    Ok(())
}

pub fn ensure_version_not_downgraded(current_tag: &str, new_tag: &str) -> Result<(), AppError> {
    let current_key = normalize_version_key(current_tag)?;
    let new_key = normalize_version_key(new_tag)?;
    if new_key < current_key {
        return Err(AppError::msg(format!(
            "New version {} must not be lower than current version {}",
            new_tag, current_tag
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct VersionKey {
    parts: Vec<u64>,
    commits_after_tag: u64,
}

impl Ord for VersionKey {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        for idx in 0..len {
            let left = self.parts.get(idx).copied().unwrap_or(0);
            let right = other.parts.get(idx).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        self.commits_after_tag.cmp(&other.commits_after_tag)
    }
}

impl PartialOrd for VersionKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn normalize_version_key(tag: &str) -> Result<VersionKey, AppError> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Err(AppError::msg("Version tag must be non-empty"));
    }
    let mut segments = trimmed.split('-');
    let release = segments.next().unwrap_or(trimmed);
    let commits_after_tag = segments
        .next()
        .and_then(|distance| distance.parse::<u64>().ok())
        .unwrap_or(0);
    let mut parts = Vec::new();
    for part in release.split('.') {
        if part.is_empty() {
            return Err(AppError::msg(format!(
                "Version tag {tag} contains an empty numeric segment"
            )));
        }
        parts.push(part.parse::<u64>().map_err(|_| {
            AppError::msg(format!(
                "Version tag {tag} must start with dot-separated numeric segments"
            ))
        })?);
    }
    Ok(VersionKey {
        parts,
        commits_after_tag,
    })
}

fn resolve_volume_source(
    source: &str,
    config_root: &Path,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf, AppError> {
    if source.eq_ignore_ascii_case("runtime") {
        return runtime_dir.map(|p| p.to_path_buf()).ok_or_else(|| {
            AppError::msg("volume source \"runtime\" requires running-config path")
        });
    }
    if let Some(stripped) = source.strip_prefix("runtime/") {
        let base = runtime_dir.ok_or_else(|| {
            AppError::msg("volume source prefixed with \"runtime/\" requires running-config path")
        })?;
        return Ok(base.join(stripped));
    }
    let path = Path::new(source);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(config_root.join(path))
    }
}

pub fn extract_tag(image_ref: &str) -> Result<String, AppError> {
    let without_digest = image_ref
        .split_once('@')
        .map(|(name, _)| name)
        .unwrap_or(image_ref);
    let Some(tag) = image_tag(without_digest) else {
        return Err(AppError::msg("Invalid image reference: missing tag"));
    };
    if tag.trim().is_empty() {
        return Err(AppError::msg(
            "Invalid image reference: tag must be non-empty",
        ));
    }
    Ok(tag.to_string())
}

fn image_tag(image_ref_without_digest: &str) -> Option<&str> {
    let tag_separator = image_ref_without_digest.rfind(':')?;
    let last_path_separator = image_ref_without_digest.rfind('/');
    if last_path_separator.is_some_and(|path_separator| tag_separator < path_separator) {
        return None;
    }
    Some(&image_ref_without_digest[tag_separator + 1..])
}

pub fn extract_digest(image_ref: &str) -> Option<String> {
    image_ref
        .split_once('@')
        .and_then(|(_, digest)| digest.strip_prefix("sha256:"))
        .map(|value| value.trim().to_string())
}

pub fn normalize_digest(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let hex = trimmed
        .strip_prefix("sha256:")
        .map(|s| s.trim())
        .unwrap_or(trimmed);
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

pub fn update_manifest_image_reference(
    content: &str,
    target_container: Option<&str>,
    new_ref: &str,
) -> Result<ManifestImageUpdate, AppError> {
    validate_image_reference(new_ref)?;
    let mut root: Value = serde_yaml::from_str(content)
        .map_err(|err| AppError::msg(format!("Failed to parse manifest YAML: {err}")))?;
    update_multi_container_manifest(&mut root, target_container, new_ref)
}

pub fn current_manifest_image_reference(
    content: &str,
    target_container: Option<&str>,
) -> Result<(String, String), AppError> {
    let root: Value = serde_yaml::from_str(content)
        .map_err(|err| AppError::msg(format!("Failed to parse manifest YAML: {err}")))?;
    current_multi_container_image_reference(&root, target_container)
}

pub fn current_manifest_daemon_base_url(content: &str) -> Option<String> {
    let root: Value = serde_yaml::from_str(content).ok()?;
    root.get("deployment")
        .and_then(|v| v.get("daemonBaseUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub struct ManifestImageUpdate {
    pub rendered: String,
}

fn update_multi_container_manifest(
    root: &mut Value,
    target_container: Option<&str>,
    new_ref: &str,
) -> Result<ManifestImageUpdate, AppError> {
    let (target, _) = current_multi_container_image_reference(root, target_container)?;

    let containers = root
        .get_mut("containers")
        .and_then(Value::as_sequence_mut)
        .ok_or_else(|| AppError::msg("containers must be a YAML sequence"))?;
    let mut previous_ref = None;
    for container in containers {
        let Some(mapping) = container.as_mapping_mut() else {
            continue;
        };
        let name = mapping_get_str(mapping, "name");
        if name.as_deref() != Some(target.as_str()) {
            continue;
        }
        let current = mapping_get_str(mapping, "image")
            .ok_or_else(|| AppError::msg(format!("container {target} missing image")))?;
        previous_ref = Some(current);
        mapping.insert(
            Value::String("image".to_string()),
            Value::String(new_ref.to_string()),
        );
        break;
    }
    let previous_ref = previous_ref.ok_or_else(|| {
        AppError::msg(format!(
            "multi-container manifest does not define container {target}"
        ))
    })?;
    let rendered = serde_yaml::to_string(root)
        .map_err(|err| AppError::msg(format!("Failed to render manifest YAML: {err}")))?;
    let _ = previous_ref;
    let _ = target;
    Ok(ManifestImageUpdate { rendered })
}

fn current_multi_container_image_reference(
    root: &Value,
    target_container: Option<&str>,
) -> Result<(String, String), AppError> {
    let target = if let Some(target) = target_container {
        target.to_string()
    } else {
        return Err(AppError::msg(
            "suggest-image requires --container to choose the target container",
        ));
    };

    container_image_reference_by_name(root, &target)
}

fn container_image_reference_by_name(
    root: &Value,
    target: &str,
) -> Result<(String, String), AppError> {
    let containers = root
        .get("containers")
        .and_then(Value::as_sequence)
        .ok_or_else(|| AppError::msg("containers must be a YAML sequence"))?;
    for container in containers {
        let Some(mapping) = container.as_mapping() else {
            continue;
        };
        let name = mapping_get_str(mapping, "name");
        if name.as_deref() != Some(target) {
            continue;
        }
        let current = mapping_get_str(mapping, "image")
            .ok_or_else(|| AppError::msg(format!("container {target} missing image")))?;
        return Ok((target.to_string(), current));
    }
    Err(AppError::msg(format!(
        "multi-container manifest does not define container {target}"
    )))
}

fn mapping_get_str(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(Value::String(key.to_string()))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        build_manifest, current_manifest_daemon_base_url, current_manifest_image_reference,
        ensure_version_not_downgraded, extract_digest, extract_tag, normalize_digest,
        update_manifest_image_reference, validate_image_reference, validate_tagged_image,
    };
    use std::path::Path;

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn image(name: &str) -> String {
        format!("registry.example/{name}:tag-a@sha256:{DIGEST}")
    }

    #[test]
    fn version_check_allows_next_patch_after_git_describe_tag() {
        ensure_version_not_downgraded("1000.0-5-gabcdef1234", "1000.1").unwrap();
    }

    #[test]
    fn version_check_rejects_same_base_behind_git_describe_tag() {
        assert!(ensure_version_not_downgraded("1000.0-5-gabcdef1234", "1000.0").is_err());
    }

    #[test]
    fn version_check_compares_numeric_segments() {
        ensure_version_not_downgraded("1000.9", "1000.10").unwrap();
        assert!(ensure_version_not_downgraded("1000.10", "1000.9").is_err());
    }

    #[test]
    fn manifest_rejects_unknown_container_fields() {
        let raw = r#"
deployment:
  name: demo
containers:
  - name: demo
    unexpectedField: true
    image: registry.example/demo:tag-a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

        assert!(build_manifest(Path::new("."), None, raw, Path::new("stow.yaml")).is_err());
    }

    #[test]
    fn suggest_image_requires_explicit_container() {
        let raw = r#"
deployment:
  name: demo
containers:
  - name: demo
    image: registry.example/demo:tag-a@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
"#;

        assert!(current_manifest_image_reference(raw, None).is_err());
        assert!(current_manifest_image_reference(raw, Some("demo")).is_ok());
    }

    #[test]
    fn manifest_translates_container_options_to_docker_flags_in_order() {
        let raw = format!(
            r#"
deployment:
  name: demo
  daemonBaseUrl: " https://daemon.example:17403 "
containers:
  - name: " api "
    image: "  {image}  "
    restartPolicy: unless-stopped
    memory: 4g
    publish:
      - "443:19091"
      - "  "
    volumes:
      - source: runtime/api
        target: /config
        readOnly: true
        selinux: z
      - source: /var/lib/data
        target: /data
      - source: certs
        target: /certs
    args:
      - "  --flag=1  "
      - "   "
"#,
            image = image("api")
        );

        let manifest = build_manifest(
            Path::new("/cfg"),
            Some(Path::new("/run-cfg")),
            &raw,
            Path::new("stow.yaml"),
        )
        .unwrap();

        assert_eq!(manifest.deployment_name, "demo");
        assert_eq!(
            manifest.daemon_base_url.as_deref(),
            Some("https://daemon.example:17403")
        );
        assert_eq!(manifest.containers.len(), 1);
        let container = &manifest.containers[0];
        assert_eq!(container.name, "api");
        assert_eq!(container.image, image("api"));
        assert_eq!(
            container.docker_flags,
            vec![
                "--restart",
                "unless-stopped",
                "--memory",
                "4g",
                "--publish",
                "443:19091",
                "--volume",
                "/run-cfg/api:/config:z,ro",
                "--volume",
                "/var/lib/data:/data",
                "--volume",
                "/cfg/certs:/certs",
            ]
        );
        assert_eq!(container.app_args, vec!["--flag=1"]);
    }

    #[test]
    fn manifest_resolves_bare_runtime_volume_source_to_running_config() {
        let raw = format!(
            r#"
deployment:
  name: demo
containers:
  - name: api
    image: {image}
    volumes:
      - source: RUNTIME
        target: /config
"#,
            image = image("api")
        );

        let manifest = build_manifest(
            Path::new("/cfg"),
            Some(Path::new("/run-cfg")),
            &raw,
            Path::new("stow.yaml"),
        )
        .unwrap();
        assert_eq!(
            manifest.containers[0].docker_flags,
            vec!["--volume", "/run-cfg:/config"]
        );
    }

    #[test]
    fn manifest_rejects_runtime_volume_without_running_config_path() {
        let raw = format!(
            r#"
deployment:
  name: demo
containers:
  - name: api
    image: {image}
    volumes:
      - source: runtime/api
        target: /config
"#,
            image = image("api")
        );

        assert!(build_manifest(Path::new("/cfg"), None, &raw, Path::new("stow.yaml")).is_err());
    }

    #[test]
    fn manifest_rejects_structural_problems() {
        let cases = [
            // empty deployment name
            format!(
                "deployment:\n  name: \"  \"\ncontainers:\n  - name: api\n    image: {}\n",
                image("api")
            ),
            // no containers
            "deployment:\n  name: demo\ncontainers: []\n".to_string(),
            // duplicate container names
            format!(
                "deployment:\n  name: demo\ncontainers:\n  - name: api\n    image: {i}\n  - name: api\n    image: {i}\n",
                i = image("api")
            ),
            // empty container name
            format!(
                "deployment:\n  name: demo\ncontainers:\n  - name: \"  \"\n    image: {}\n",
                image("api")
            ),
            // empty volume target
            format!(
                "deployment:\n  name: demo\ncontainers:\n  - name: api\n    image: {}\n    volumes:\n      - source: /data\n        target: \"  \"\n",
                image("api")
            ),
        ];
        for raw in cases {
            assert!(
                build_manifest(Path::new("."), None, &raw, Path::new("stow.yaml")).is_err(),
                "expected rejection for:\n{raw}"
            );
        }
    }

    #[test]
    fn manifest_blank_daemon_base_url_becomes_none() {
        let raw = format!(
            "deployment:\n  name: demo\n  daemonBaseUrl: \"  \"\ncontainers:\n  - name: api\n    image: {}\n",
            image("api")
        );
        let manifest = build_manifest(Path::new("."), None, &raw, Path::new("stow.yaml")).unwrap();
        assert_eq!(manifest.daemon_base_url, None);
    }

    #[test]
    fn image_reference_validation_requires_tag_and_full_digest() {
        assert!(validate_image_reference(&image("api")).is_ok());
        assert!(
            validate_image_reference(&format!("localhost:5000/api:tag-a@sha256:{DIGEST}")).is_ok()
        );
        // missing digest entirely
        assert!(validate_image_reference("registry.example/api:tag-a").is_err());
        // digest but no tag
        assert!(
            validate_image_reference(&format!("registry.example/api@sha256:{DIGEST}")).is_err()
        );
        // registry port is not a tag
        assert!(validate_image_reference(&format!("localhost:5000/api@sha256:{DIGEST}")).is_err());
        // digest too short
        assert!(validate_image_reference("registry.example/api:tag-a@sha256:abc123").is_err());
        // digest with non-hex characters
        assert!(validate_image_reference(&format!(
            "registry.example/api:tag-a@sha256:{}",
            "z".repeat(64)
        ))
        .is_err());
    }

    #[test]
    fn tagged_image_validation_rejects_digest_and_missing_tag() {
        assert!(validate_tagged_image("registry.example/api:tag-a").is_ok());
        assert!(validate_tagged_image("localhost:5000/api:tag-a").is_ok());
        assert!(validate_tagged_image(&image("api")).is_err());
        assert!(validate_tagged_image("registry.example/api").is_err());
        assert!(validate_tagged_image("localhost:5000/api").is_err());
        assert!(validate_tagged_image("registry.example/api: ").is_err());
    }

    #[test]
    fn tag_and_digest_extraction() {
        assert_eq!(extract_tag(&image("api")).unwrap(), "tag-a");
        assert_eq!(extract_tag("registry.example/api:tag-b").unwrap(), "tag-b");
        assert_eq!(extract_tag("localhost:5000/api:tag-c").unwrap(), "tag-c");
        assert!(extract_tag("registry.example/api").is_err());
        assert!(extract_tag("localhost:5000/api").is_err());
        assert!(extract_tag("registry.example/api: ").is_err());

        assert_eq!(extract_digest(&image("api")).as_deref(), Some(DIGEST));
        assert_eq!(extract_digest("registry.example/api:tag-a"), None);
    }

    #[test]
    fn digest_normalization_accepts_optional_prefix_and_lowercases() {
        assert_eq!(normalize_digest(DIGEST).as_deref(), Some(DIGEST));
        assert_eq!(
            normalize_digest(&format!("  sha256:{}  ", DIGEST.to_uppercase())).as_deref(),
            Some(DIGEST)
        );
        assert_eq!(normalize_digest(""), None);
        assert_eq!(normalize_digest("sha256:abc"), None);
        assert_eq!(normalize_digest(&"g".repeat(64)), None);
    }

    #[test]
    fn version_check_allows_equal_versions_and_pads_missing_segments() {
        ensure_version_not_downgraded("10.2", "10.2").unwrap();
        // 10.2 == 10.2.0
        ensure_version_not_downgraded("10.2.0", "10.2").unwrap();
        ensure_version_not_downgraded("10.2", "10.2.0").unwrap();
        assert!(ensure_version_not_downgraded("10.2.1", "10.2").is_err());
    }

    #[test]
    fn version_check_rejects_non_numeric_and_empty_tags() {
        assert!(ensure_version_not_downgraded("abc", "10.0").is_err());
        assert!(ensure_version_not_downgraded("10.0", "abc").is_err());
        assert!(ensure_version_not_downgraded("", "10.0").is_err());
        assert!(ensure_version_not_downgraded("10..0", "10.0").is_err());
    }

    #[test]
    fn version_check_treats_non_numeric_distance_as_zero() {
        // "10.0-rc" parses the dash suffix as distance 0, so it equals "10.0"
        ensure_version_not_downgraded("10.0-rc", "10.0").unwrap();
        ensure_version_not_downgraded("10.0", "10.0-rc").unwrap();
    }

    #[test]
    fn manifest_image_update_rewrites_only_target_container() {
        let raw = format!(
            "deployment:\n  name: demo\ncontainers:\n- name: api\n  image: {api}\n  memory: 4g\n- name: worker\n  image: {worker}\n",
            api = image("api"),
            worker = image("worker")
        );
        let new_ref = format!("registry.example/api:tag-b@sha256:{}", "b".repeat(64));

        let update = update_manifest_image_reference(&raw, Some("api"), &new_ref).unwrap();
        assert!(update.rendered.contains(&new_ref));
        assert!(!update.rendered.contains(&image("api")));
        // untouched container and other fields survive the rewrite
        assert!(update.rendered.contains(&image("worker")));
        assert!(update.rendered.contains("memory: 4g"));
    }

    #[test]
    fn manifest_image_update_rejects_unknown_container_and_invalid_ref() {
        let raw = format!(
            "deployment:\n  name: demo\ncontainers:\n- name: api\n  image: {}\n",
            image("api")
        );
        let new_ref = format!("registry.example/api:tag-b@sha256:{}", "b".repeat(64));
        assert!(update_manifest_image_reference(&raw, Some("missing"), &new_ref).is_err());
        assert!(update_manifest_image_reference(&raw, None, &new_ref).is_err());
        assert!(
            update_manifest_image_reference(&raw, Some("api"), "registry.example/api:tag-b")
                .is_err()
        );
    }

    #[test]
    fn current_image_reference_returns_container_and_ref() {
        let raw = format!(
            "deployment:\n  name: demo\ncontainers:\n- name: api\n  image: {}\n",
            image("api")
        );
        let (container, current) = current_manifest_image_reference(&raw, Some("api")).unwrap();
        assert_eq!(container, "api");
        assert_eq!(current, image("api"));
    }

    #[test]
    fn daemon_base_url_extraction_from_raw_manifest() {
        assert_eq!(
            current_manifest_daemon_base_url(
                "deployment:\n  name: demo\n  daemonBaseUrl: \" https://d.example \"\n"
            )
            .as_deref(),
            Some("https://d.example")
        );
        assert_eq!(
            current_manifest_daemon_base_url("deployment:\n  name: demo\n"),
            None
        );
        assert_eq!(
            current_manifest_daemon_base_url("deployment:\n  name: demo\n  daemonBaseUrl: \"\"\n"),
            None
        );
        assert_eq!(current_manifest_daemon_base_url("not: yaml: ["), None);
    }
}
