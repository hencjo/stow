use crate::app_error::AppError;
use crate::cli::ReconcileOptions;
use crate::docker::{
    apply_plan, ensure_images_for_manifest, inspect_deployment_containers, plan_reconciliation,
    plan_summary, prune_stale_images, verify_plan, ObservedContainer, ReconcilePlan,
};
use crate::fs_utils::CleanupPath;
use crate::fs_utils::DirLock;
use crate::hashing::{compute_deployment_hashes, DeploymentHashes};
use crate::manifest::{load_manifest, DeploymentManifest};
use crate::state::Context;
use crate::util::log;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::sops::decrypt_config;

pub fn run_reconcile_once(ctx: &Context, opts: &ReconcileOptions) -> Result<(), AppError> {
    let _lock = DirLock::acquire(&ctx.lock_dir)?;
    let (repo_guard, desired_dir, revision) = ctx.download_repo()?;
    let secret_files: BTreeSet<PathBuf> = decrypt_config(ctx, &desired_dir, repo_guard.path())?;
    let staging_guard = ctx.stage_config(&desired_dir)?;
    reconcile(ctx, staging_guard, revision, opts, &secret_files)
}

pub fn compute_deployment_hash_for_revision(
    ctx: &Context,
    revision: &str,
) -> Result<DeploymentHashes, AppError> {
    let (repo_guard, desired_dir, _) = ctx.download_repo_at_revision(Some(revision))?;
    let secret_files: BTreeSet<PathBuf> = decrypt_config(ctx, &desired_dir, repo_guard.path())?;
    let _desired = load_manifest(&desired_dir, Some(ctx.state_dir()))?;
    compute_deployment_hashes(&desired_dir, &secret_files)
}

pub fn reconcile(
    ctx: &Context,
    mut staging: CleanupPath,
    revision: String,
    opts: &ReconcileOptions,
    secret_files: &std::collections::BTreeSet<std::path::PathBuf>,
) -> Result<(), AppError> {
    let desired = load_manifest(staging.path(), Some(ctx.state_dir()))?;
    let hashes = compute_deployment_hashes(staging.path(), secret_files)?;
    let new_hash = hashes.deployment_hash.clone();
    let observed = inspect_deployment_containers(&desired.deployment_name)?;
    let plan = plan_reconciliation(&desired, &observed, &new_hash);

    emit_plan(opts, &desired, &observed, &plan, &revision, &hashes)?;

    if opts.dry_run {
        log("Dry-run requested; not applying changes.");
        return Ok(());
    }

    let previous_hash = ctx.read_current_hash()?;
    if previous_hash.as_deref() == Some(new_hash.as_str())
        && plan
            .operations
            .iter()
            .all(|op| matches!(op, crate::docker::ContainerOperation::NoOp(_)))
    {
        log(&format!(
            "Already running desired deployment {} ({}). Nothing to do.",
            desired.deployment_name, new_hash
        ));
        return Ok(());
    }

    ensure_images_for_manifest(&desired)?;
    let previous_manifest = ctx.load_running_manifest().transpose()?;

    ctx.rotate_state_dirs(&mut staging, &new_hash)?;
    ctx.write_metadata(&revision, &new_hash, &desired.deployment_name)?;

    let desired_from_state = load_manifest(ctx.state_dir(), Some(ctx.state_dir()))?;
    let apply_result = apply_plan(&desired_from_state, &plan, &new_hash)
        .and_then(|_| verify_plan(&desired_from_state, &new_hash));

    if let Err(err) = apply_result {
        eprintln!("[stow][error] Failed to apply desired deployment: {err}. Attempting rollback.");
        rollback(
            ctx,
            previous_manifest
                .as_ref()
                .map(|m| m.deployment_name.as_str()),
        )?;
        return Err(AppError::msg(
            "Reconciliation failed; reverted to previous deployment",
        ));
    }

    prune_images_after_success(
        ctx,
        &desired_from_state,
        previous_manifest.as_ref(),
        previous_hash,
    )?;
    ctx.cleanup_previous_dir()?;
    log("Reconcile completed successfully.");
    Ok(())
}

fn prune_images_after_success(
    ctx: &Context,
    current_manifest: &DeploymentManifest,
    previous_manifest: Option<&DeploymentManifest>,
    previous_hash: Option<String>,
) -> Result<(), AppError> {
    let mut keep_hashes = BTreeSet::new();
    if let Some(current_hash) = ctx.read_current_hash()? {
        keep_hashes.insert(current_hash);
    }
    if let Some(previous_hash) = previous_hash {
        keep_hashes.insert(previous_hash);
    }

    let mut keep_manifests = vec![current_manifest.clone()];
    if let Some(previous_manifest) = previous_manifest {
        keep_manifests.push(previous_manifest.clone());
    }
    let candidate_manifests = ctx.load_snapshot_manifests_excluding(&keep_hashes)?;
    if candidate_manifests.is_empty() {
        log("Image prune found no older stow snapshots.");
        return Ok(());
    }
    if let Err(err) = prune_stale_images(&keep_manifests, &candidate_manifests) {
        log(&format!(
            "Image prune failed; deployment remains successful: {err}"
        ));
    }
    Ok(())
}

pub fn rollback(ctx: &Context, previous_deployment_name: Option<&str>) -> Result<(), AppError> {
    ctx.restore_previous_state()?;
    let desired = load_manifest(ctx.state_dir(), Some(ctx.state_dir()))?;
    let revision = ctx.read_state_revision()?;
    let config_hash = ctx
        .read_current_hash()?
        .ok_or_else(|| AppError::msg("Rollback state is missing .config-sha256"))?;

    let observed = inspect_deployment_containers(&desired.deployment_name)?;
    let plan = plan_reconciliation(&desired, &observed, &config_hash);
    log(&format!(
        "Rollback plan for {}: {}",
        desired.deployment_name,
        plan_summary(&plan)
    ));
    let _ = revision;
    apply_plan(&desired, &plan, &config_hash)?;
    verify_plan(&desired, &config_hash)?;
    log(&format!(
        "Rollback succeeded{}.",
        previous_deployment_name
            .map(|name| format!(" to {name}"))
            .unwrap_or_default()
    ));
    Ok(())
}

fn emit_plan(
    opts: &ReconcileOptions,
    desired: &DeploymentManifest,
    observed: &[ObservedContainer],
    plan: &ReconcilePlan,
    revision: &str,
    hashes: &DeploymentHashes,
) -> Result<(), AppError> {
    if opts.plan_json {
        let payload = PlanOutput {
            deployment: &desired.deployment_name,
            revision,
            hashes,
            dry_run: opts.dry_run,
            desired,
            observed,
            plan,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|err| AppError::msg(format!("failed to serialize plan JSON: {err}")))?
        );
    } else {
        log(&format!(
            "Planned operations for {}: {} (deployment-hash={})",
            desired.deployment_name,
            plan_summary(plan),
            hashes.deployment_hash
        ));
        log(&format!(
            "Hash parts: manifest={}, config={}, secrets={}",
            hashes.manifest_hash, hashes.config_hash, hashes.secrets_hash
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct PlanOutput<'a> {
    deployment: &'a str,
    revision: &'a str,
    hashes: &'a DeploymentHashes,
    dry_run: bool,
    desired: &'a DeploymentManifest,
    observed: &'a [ObservedContainer],
    plan: &'a ReconcilePlan,
}
