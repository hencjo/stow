use crate::app_error::AppError;
use crate::command::{capture_command, run_command};
use crate::manifest::{DeploymentManifest, DesiredContainerSpec};
use crate::util::log;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const LABEL_DEPLOYMENT: &str = "stow.deployment";
const LABEL_VERSIONED_HASH: &str = "stow.hash";
const HASH_VERSION: &str = "v1";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
const VERIFY_INTERVAL: Duration = Duration::from_secs(2);
const VERIFY_STABLE_FOR: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize)]
pub struct ObservedContainer {
    pub name: String,
    pub running: bool,
    pub restarting: bool,
    pub restart_count: u64,
    pub health_status: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "container")]
pub enum ContainerOperation {
    Replace(String),
    Delete(String),
    NoOp(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcilePlan {
    pub operations: Vec<ContainerOperation>,
}

pub fn ensure_image_present(image: &str) -> Result<(), AppError> {
    if image_present(image)? {
        log(&format!(
            "Image {image} already present locally; skipping pull."
        ));
        return Ok(());
    }
    log(&format!("Pulling image {image}"));
    let status = Command::new("docker").arg("pull").arg(image).status()?;
    if !status.success() {
        return Err(AppError::msg(format!("docker pull failed for {image}")));
    }
    Ok(())
}

pub fn inspect_deployment_containers(deployment: &str) -> Result<Vec<ObservedContainer>, AppError> {
    let filter = format!("label={LABEL_DEPLOYMENT}={deployment}");
    let output = capture_command(
        "docker",
        &[
            OsStr::new("ps"),
            OsStr::new("-a"),
            OsStr::new("--filter"),
            OsStr::new(&filter),
            OsStr::new("--format"),
            OsStr::new("{{.ID}}"),
        ],
    )?;
    let mut out = Vec::new();
    for id in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(container) = inspect_container_by_id(id)? {
            out.push(container);
        }
    }
    Ok(out)
}

fn inspect_container_by_id(id: &str) -> Result<Option<ObservedContainer>, AppError> {
    let output = Command::new("docker").arg("inspect").arg(id).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| AppError::msg(format!("docker inspect parse failed: {err}")))?;
    parse_inspect_entry(value.into_iter().next())
}

fn parse_inspect_entry(entry: Option<Value>) -> Result<Option<ObservedContainer>, AppError> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    let name = entry
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let running = entry
        .get("State")
        .and_then(|v| v.get("Running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let restarting = entry
        .get("State")
        .and_then(|v| v.get("Restarting"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let restart_count = entry
        .get("RestartCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let health_status = entry
        .get("State")
        .and_then(|v| v.get("Health"))
        .and_then(|v| v.get("Status"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let labels = entry
        .get("Config")
        .and_then(|v| v.get("Labels"))
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    Ok(Some(ObservedContainer {
        name,
        running,
        restarting,
        restart_count,
        health_status,
        labels,
    }))
}

pub fn plan_reconciliation(
    desired: &DeploymentManifest,
    observed: &[ObservedContainer],
    config_hash: &str,
) -> ReconcilePlan {
    let mut operations = Vec::new();
    let observed_by_name = observed
        .iter()
        .map(|container| (container.name.as_str(), container))
        .collect::<BTreeMap<_, _>>();

    for container in &desired.containers {
        match observed_by_name.get(container.name.as_str()) {
            Some(existing) if container_matches_existing(desired, existing, config_hash) => {
                operations.push(ContainerOperation::NoOp(container.name.clone()));
            }
            None | Some(_) => operations.push(ContainerOperation::Replace(container.name.clone())),
        }
    }

    for existing in observed {
        if desired
            .containers
            .iter()
            .all(|item| item.name != existing.name)
        {
            operations.push(ContainerOperation::Delete(existing.name.clone()));
        }
    }

    ReconcilePlan { operations }
}

fn container_matches_existing(
    desired: &DeploymentManifest,
    existing: &ObservedContainer,
    config_hash: &str,
) -> bool {
    existing.running
        && existing.labels.get(LABEL_DEPLOYMENT).map(String::as_str)
            == Some(desired.deployment_name.as_str())
        && existing
            .labels
            .get(LABEL_VERSIONED_HASH)
            .map(String::as_str)
            == Some(versioned_hash(config_hash).as_str())
}

pub fn apply_plan(
    desired: &DeploymentManifest,
    plan: &ReconcilePlan,
    config_hash: &str,
) -> Result<(), AppError> {
    for operation in &plan.operations {
        match operation {
            ContainerOperation::Delete(name) | ContainerOperation::Replace(name) => {
                stop_and_remove_container(name)?
            }
            ContainerOperation::NoOp(_) => {}
        }
    }

    for operation in &plan.operations {
        match operation {
            ContainerOperation::Replace(name) => {
                let spec = desired
                    .containers
                    .iter()
                    .find(|container| &container.name == name)
                    .ok_or_else(|| {
                        AppError::msg(format!("desired container {name} missing from manifest"))
                    })?;
                start_container(spec, &desired.deployment_name, config_hash)?;
            }
            ContainerOperation::NoOp(_) | ContainerOperation::Delete(_) => {}
        }
    }
    Ok(())
}

pub fn verify_plan(desired: &DeploymentManifest, config_hash: &str) -> Result<(), AppError> {
    let deadline = Instant::now() + VERIFY_TIMEOUT;
    let mut stable_since = None;

    loop {
        match verify_deployment_once(desired, config_hash) {
            Ok(()) => {
                let first_stable = stable_since.get_or_insert_with(Instant::now);
                let stable_for = first_stable.elapsed();
                if stable_for >= VERIFY_STABLE_FOR {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(AppError::msg(format!(
                        "deployment verification failed: deployment did not remain stable for {} seconds",
                        VERIFY_STABLE_FOR.as_secs()
                    )));
                }
                log(&format!(
                    "Waiting for deployment to remain stable for {} seconds ({}/{} seconds).",
                    VERIFY_STABLE_FOR.as_secs(),
                    stable_for.as_secs(),
                    VERIFY_STABLE_FOR.as_secs()
                ));
            }
            Err(err) => {
                stable_since = None;
                if Instant::now() >= deadline {
                    return Err(err);
                }
                log(&format!("Waiting for deployment verification: {err}"));
            }
        }

        thread::sleep(VERIFY_INTERVAL);
    }
}

fn verify_deployment_once(desired: &DeploymentManifest, config_hash: &str) -> Result<(), AppError> {
    let observed = inspect_deployment_containers(&desired.deployment_name)?;
    for container in &desired.containers {
        let Some(existing) = observed.iter().find(|item| item.name == container.name) else {
            return Err(AppError::msg(format!(
                "deployment verification failed: {} is missing",
                container.name
            )));
        };
        verify_container_started(desired, existing, config_hash)?;
    }
    Ok(())
}

fn verify_container_started(
    desired: &DeploymentManifest,
    existing: &ObservedContainer,
    config_hash: &str,
) -> Result<(), AppError> {
    if existing.labels.get(LABEL_DEPLOYMENT).map(String::as_str)
        != Some(desired.deployment_name.as_str())
    {
        return Err(AppError::msg(format!(
            "deployment verification failed: {} has wrong deployment label",
            existing.name
        )));
    }
    if existing
        .labels
        .get(LABEL_VERSIONED_HASH)
        .map(String::as_str)
        != Some(versioned_hash(config_hash).as_str())
    {
        return Err(AppError::msg(format!(
            "deployment verification failed: {} has wrong stow hash",
            existing.name
        )));
    }
    if !existing.running {
        return Err(AppError::msg(format!(
            "deployment verification failed: {} is not running",
            existing.name
        )));
    }
    if existing.restarting {
        return Err(AppError::msg(format!(
            "deployment verification failed: {} is restarting",
            existing.name
        )));
    }
    if existing.restart_count > 0 {
        return Err(AppError::msg(format!(
            "deployment verification failed: {} has restarted {} time(s)",
            existing.name, existing.restart_count
        )));
    }
    if let Some(health) = existing.health_status.as_deref() {
        if health != "healthy" {
            return Err(AppError::msg(format!(
                "deployment verification failed: {} health is {health}",
                existing.name
            )));
        }
    }
    Ok(())
}

pub fn ensure_images_for_manifest(desired: &DeploymentManifest) -> Result<(), AppError> {
    for container in &desired.containers {
        ensure_image_present(&container.image)?;
    }
    Ok(())
}

pub fn prune_stale_images(
    keep_manifests: &[DeploymentManifest],
    candidate_manifests: &[DeploymentManifest],
) -> Result<(), AppError> {
    let keep_images = keep_image_references(keep_manifests);
    let keep_image_ids = inspect_image_ids(&keep_images)?;
    let used_image_ids = inspect_container_image_ids()?;
    let candidate_images = keep_image_references(candidate_manifests);
    let candidate_image_ids = inspect_image_ids(&candidate_images)?;
    let prune_image_ids = stale_image_ids(&keep_image_ids, &used_image_ids, &candidate_image_ids);

    let mut pruned = 0usize;
    for image_id in prune_image_ids {
        log(&format!("Pruning stale image {image_id}"));
        let status = Command::new("docker")
            .arg("image")
            .arg("rm")
            .arg(&image_id)
            .status()?;
        if status.success() {
            pruned += 1;
        } else {
            log(&format!(
                "Skipping stale image {image_id}; docker refused to remove it."
            ));
        }
    }

    if pruned == 0 {
        log("Image prune found no stale images.");
    } else {
        log(&format!("Image prune removed {pruned} stale image(s)."));
    }
    Ok(())
}

fn keep_image_references(manifests: &[DeploymentManifest]) -> BTreeSet<String> {
    manifests
        .iter()
        .flat_map(|manifest| manifest.containers.iter())
        .map(|container| container.image.clone())
        .collect()
}

fn inspect_image_ids(images: &BTreeSet<String>) -> Result<BTreeSet<String>, AppError> {
    let mut ids = BTreeSet::new();
    for image in images {
        match inspect_image_id(image) {
            Ok(Some(id)) => {
                ids.insert(id);
            }
            Ok(None) => {}
            Err(err) => log(&format!(
                "Could not inspect kept image {image}; not using it as prune protection: {err}"
            )),
        }
    }
    Ok(ids)
}

fn inspect_image_id(image: &str) -> Result<Option<String>, AppError> {
    let output = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| AppError::msg(format!("docker image inspect parse failed: {err}")))?;
    Ok(value
        .into_iter()
        .next()
        .and_then(|entry| entry.get("Id").and_then(Value::as_str).map(str::to_string)))
}

fn inspect_container_image_ids() -> Result<BTreeSet<String>, AppError> {
    let output = capture_command(
        "docker",
        &[
            OsStr::new("ps"),
            OsStr::new("-a"),
            OsStr::new("--format"),
            OsStr::new("{{.ID}}"),
        ],
    )?;
    let mut ids = BTreeSet::new();
    for id in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let inspect = Command::new("docker").arg("inspect").arg(id).output()?;
        if !inspect.status.success() {
            continue;
        }
        let value: Vec<Value> = serde_json::from_slice(&inspect.stdout)
            .map_err(|err| AppError::msg(format!("docker inspect parse failed: {err}")))?;
        if let Some(image_id) = value.into_iter().next().and_then(|entry| {
            entry
                .get("Image")
                .and_then(Value::as_str)
                .map(str::to_string)
        }) {
            ids.insert(image_id);
        }
    }
    Ok(ids)
}

fn stale_image_ids(
    keep_image_ids: &BTreeSet<String>,
    used_image_ids: &BTreeSet<String>,
    candidate_image_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    candidate_image_ids
        .iter()
        .filter(|id| !keep_image_ids.contains(*id) && !used_image_ids.contains(*id))
        .cloned()
        .collect()
}

fn image_present(image: &str) -> Result<bool, AppError> {
    let status = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .status()?;
    Ok(status.success())
}

pub fn stop_and_remove_container(name: &str) -> Result<(), AppError> {
    if !container_exists(name)? {
        return Ok(());
    }
    log(&format!("Stopping container {name}"));
    let mut stop_cmd = Command::new("docker");
    stop_cmd.arg("stop").arg(name);
    run_command(stop_cmd)?;
    log(&format!("Removing container {name}"));
    let mut rm_cmd = Command::new("docker");
    rm_cmd.arg("rm").arg(name);
    run_command(rm_cmd)?;
    Ok(())
}

fn container_exists(name: &str) -> Result<bool, AppError> {
    let filter = format!("name=^/{name}$");
    let output = capture_command(
        "docker",
        &[
            OsStr::new("ps"),
            OsStr::new("-a"),
            OsStr::new("--filter"),
            OsStr::new(&filter),
            OsStr::new("--format"),
            OsStr::new("{{.Names}}"),
        ],
    )?;
    Ok(output.lines().any(|line| line.trim() == name))
}

fn start_container(
    spec: &DesiredContainerSpec,
    deployment_name: &str,
    config_hash: &str,
) -> Result<(), AppError> {
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--detach")
        .arg("--name")
        .arg(&spec.name)
        .arg("--label")
        .arg(format!("{LABEL_DEPLOYMENT}={deployment_name}"))
        .arg("--label")
        .arg(format!(
            "{LABEL_VERSIONED_HASH}={}",
            versioned_hash(config_hash)
        ));
    for flag in &spec.docker_flags {
        cmd.arg(flag);
    }
    cmd.arg(&spec.image);
    for arg in &spec.app_args {
        cmd.arg(arg);
    }
    log(&format!("Starting container {}", spec.name));
    run_command(cmd)
}

pub fn plan_summary(plan: &ReconcilePlan) -> String {
    plan.operations
        .iter()
        .map(|op| match op {
            ContainerOperation::Replace(name) => format!("replace:{name}"),
            ContainerOperation::Delete(name) => format!("delete:{name}"),
            ContainerOperation::NoOp(name) => format!("noop:{name}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn versioned_hash(config_hash: &str) -> String {
    format!("{HASH_VERSION}:{config_hash}")
}

#[cfg(test)]
mod tests {
    use super::{
        keep_image_references, parse_inspect_entry, plan_reconciliation, plan_summary,
        stale_image_ids, verify_container_started, versioned_hash, ContainerOperation,
        ObservedContainer,
    };
    use crate::manifest::{DeploymentManifest, DesiredContainerSpec};
    use std::collections::{BTreeMap, BTreeSet};

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn manifest(deployment: &str, container_names: &[&str]) -> DeploymentManifest {
        DeploymentManifest {
            deployment_name: deployment.to_string(),
            daemon_base_url: None,
            containers: container_names
                .iter()
                .map(|name| DesiredContainerSpec {
                    name: name.to_string(),
                    image: format!("registry.example/{name}:1@sha256:{}", "a".repeat(64)),
                    docker_flags: Vec::new(),
                    app_args: Vec::new(),
                })
                .collect(),
        }
    }

    fn observed(name: &str, deployment: &str, hash: &str) -> ObservedContainer {
        let mut labels = BTreeMap::new();
        labels.insert("stow.deployment".to_string(), deployment.to_string());
        labels.insert("stow.hash".to_string(), format!("v1:{hash}"));
        ObservedContainer {
            name: name.to_string(),
            running: true,
            restarting: false,
            restart_count: 0,
            health_status: None,
            labels,
        }
    }

    fn op_summary(ops: &[ContainerOperation]) -> Vec<String> {
        ops.iter()
            .map(|op| match op {
                ContainerOperation::Replace(name) => format!("replace:{name}"),
                ContainerOperation::Delete(name) => format!("delete:{name}"),
                ContainerOperation::NoOp(name) => format!("noop:{name}"),
            })
            .collect()
    }

    #[test]
    fn versioned_hash_prefixes_v1() {
        assert_eq!(versioned_hash("abc"), "v1:abc");
    }

    #[test]
    fn plan_keeps_matching_container_as_noop() {
        let desired = manifest("demo", &["api"]);
        let plan = plan_reconciliation(&desired, &[observed("api", "demo", "h1")], "h1");
        assert_eq!(op_summary(&plan.operations), vec!["noop:api"]);
    }

    #[test]
    fn plan_replaces_missing_stopped_or_stale_containers() {
        let desired = manifest("demo", &["api"]);

        // not observed at all
        let plan = plan_reconciliation(&desired, &[], "h1");
        assert_eq!(op_summary(&plan.operations), vec!["replace:api"]);

        // observed but stopped
        let mut stopped = observed("api", "demo", "h1");
        stopped.running = false;
        let plan = plan_reconciliation(&desired, &[stopped], "h1");
        assert_eq!(op_summary(&plan.operations), vec!["replace:api"]);

        // observed with a different deployment hash
        let plan = plan_reconciliation(&desired, &[observed("api", "demo", "old")], "h1");
        assert_eq!(op_summary(&plan.operations), vec!["replace:api"]);

        // observed with wrong deployment label
        let plan = plan_reconciliation(&desired, &[observed("api", "other", "h1")], "h1");
        assert_eq!(op_summary(&plan.operations), vec!["replace:api"]);
    }

    #[test]
    fn plan_deletes_containers_not_in_manifest_after_desired_operations() {
        let desired = manifest("demo", &["api", "worker"]);
        let plan = plan_reconciliation(
            &desired,
            &[
                observed("api", "demo", "h1"),
                observed("legacy", "demo", "h0"),
            ],
            "h1",
        );
        assert_eq!(
            op_summary(&plan.operations),
            vec!["noop:api", "replace:worker", "delete:legacy"]
        );
    }

    #[test]
    fn plan_summary_joins_operations_with_commas() {
        let desired = manifest("demo", &["api"]);
        let plan = plan_reconciliation(&desired, &[observed("legacy", "demo", "h0")], "h1");
        assert_eq!(plan_summary(&plan), "replace:api, delete:legacy");
    }

    #[test]
    fn inspect_entry_parses_state_and_labels() {
        let entry = serde_json::json!({
            "Name": "/api",
            "RestartCount": 3,
            "State": {
                "Running": true,
                "Restarting": true,
                "Health": { "Status": "healthy" }
            },
            "Config": {
                "Labels": {
                    "stow.deployment": "demo",
                    "stow.hash": "v1:h1"
                }
            }
        });
        let container = parse_inspect_entry(Some(entry)).unwrap().unwrap();
        assert_eq!(container.name, "api");
        assert!(container.running);
        assert!(container.restarting);
        assert_eq!(container.restart_count, 3);
        assert_eq!(container.health_status.as_deref(), Some("healthy"));
        assert_eq!(
            container.labels.get("stow.deployment").map(String::as_str),
            Some("demo")
        );
    }

    #[test]
    fn inspect_entry_defaults_missing_fields() {
        let container = parse_inspect_entry(Some(serde_json::json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(container.name, "");
        assert!(!container.running);
        assert!(!container.restarting);
        assert_eq!(container.restart_count, 0);
        assert_eq!(container.health_status, None);
        assert!(container.labels.is_empty());

        assert!(parse_inspect_entry(None).unwrap().is_none());
    }

    #[test]
    fn container_verification_accepts_running_labeled_container() {
        let desired = manifest("demo", &["api"]);
        verify_container_started(&desired, &observed("api", "demo", "h1"), "h1").unwrap();

        // explicit healthy status also passes
        let mut healthy = observed("api", "demo", "h1");
        healthy.health_status = Some("healthy".to_string());
        verify_container_started(&desired, &healthy, "h1").unwrap();
    }

    #[test]
    fn container_verification_rejects_each_failure_mode() {
        let desired = manifest("demo", &["api"]);

        let wrong_deployment = observed("api", "other", "h1");
        assert!(verify_container_started(&desired, &wrong_deployment, "h1").is_err());

        let wrong_hash = observed("api", "demo", "old");
        assert!(verify_container_started(&desired, &wrong_hash, "h1").is_err());

        let mut stopped = observed("api", "demo", "h1");
        stopped.running = false;
        assert!(verify_container_started(&desired, &stopped, "h1").is_err());

        let mut restarting = observed("api", "demo", "h1");
        restarting.restarting = true;
        assert!(verify_container_started(&desired, &restarting, "h1").is_err());

        let mut restarted = observed("api", "demo", "h1");
        restarted.restart_count = 1;
        assert!(verify_container_started(&desired, &restarted, "h1").is_err());

        let mut unhealthy = observed("api", "demo", "h1");
        unhealthy.health_status = Some("starting".to_string());
        assert!(verify_container_started(&desired, &unhealthy, "h1").is_err());
    }

    #[test]
    fn stale_image_ids_keeps_current_previous_and_used_images() {
        assert_eq!(
            stale_image_ids(
                &set(&["sha256:current", "sha256:previous"]),
                &set(&["sha256:used-elsewhere"]),
                &set(&[
                    "sha256:current",
                    "sha256:previous",
                    "sha256:used-elsewhere",
                    "sha256:stale"
                ]),
            ),
            set(&["sha256:stale"])
        );
    }

    #[test]
    fn stale_image_ids_does_not_prune_image_shared_by_old_and_current_snapshots() {
        assert_eq!(
            stale_image_ids(
                &set(&["sha256:same-image"]),
                &BTreeSet::new(),
                &set(&["sha256:same-image"]),
            ),
            BTreeSet::new()
        );
    }

    #[test]
    fn keep_image_references_includes_every_container_in_descriptor() {
        let manifest = DeploymentManifest {
            deployment_name: "test".to_string(),
            daemon_base_url: None,
            containers: vec![
                DesiredContainerSpec {
                    name: "web".to_string(),
                    image: "registry.example/web:1@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                    docker_flags: Vec::new(),
                    app_args: Vec::new(),
                },
                DesiredContainerSpec {
                    name: "worker".to_string(),
                    image: "registry.example/worker:1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_string(),
                    docker_flags: Vec::new(),
                    app_args: Vec::new(),
                },
            ],
        };

        assert_eq!(
            keep_image_references(&[manifest]),
            set(&[
                "registry.example/web:1@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "registry.example/worker:1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ])
        );
    }
}
