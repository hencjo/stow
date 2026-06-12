use crate::app_error::AppError;
use crate::command::{detect_revision, download_file, ensure_command_exists, extract_archive};
use crate::fs_utils::{copy_dir_recursive, CleanupPath};
use crate::gitlab::GitLabConfig;
use crate::manifest::{load_manifest, DeploymentManifest, STOW_DEFINITION_FILE};
use crate::util::{log, rand_suffix, unique_suffix};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub struct Context {
    pub home: PathBuf,
    pub gitlab: GitLabConfig,
    pub subfolder: String,
    pub sops_bin: PathBuf,
    pub age_key_file: PathBuf,
    pub gitlab_token: String,
    pub gitlab_auth_header: String,
    pub data_dir: PathBuf,
    pub snapshot_root: PathBuf,
    pub state_dir: PathBuf,
    pub previous_dir: PathBuf,
    pub lock_dir: PathBuf,
}

impl Context {
    pub fn new(
        home: PathBuf,
        gitlab: GitLabConfig,
        subfolder: String,
        gitlab_token: String,
        gitlab_auth_header: String,
        reconcile: crate::cli::ReconcileOptions,
    ) -> Self {
        let sops_bin = reconcile
            .sops_binary
            .unwrap_or_else(|| PathBuf::from("sops"));
        Self {
            gitlab,
            subfolder,
            sops_bin,
            age_key_file: reconcile.keys_file,
            gitlab_token,
            gitlab_auth_header,
            data_dir: home.join(".stow"),
            snapshot_root: home.join(".stow").join("snapshots"),
            state_dir: home.join("running-config"),
            previous_dir: home.join("running-config.previous"),
            lock_dir: home.join(".stow").join("lock"),
            home,
        }
    }

    pub fn ensure_prereqs(&self) -> Result<(), AppError> {
        fs::create_dir_all(&self.data_dir)?;
        fs::create_dir_all(&self.snapshot_root)?;
        for cmd in ["curl", "tar", "docker"] {
            ensure_command_exists(cmd)?;
        }
        if !self.age_key_file.exists() {
            return Err(AppError::msg(format!(
                "SOPS key file missing: {}",
                self.age_key_file.display()
            )));
        }
        if self.gitlab_token.is_empty() {
            return Err(AppError::msg(
                "GitLab token missing; set gitlabToken in the config file",
            ));
        }
        let resolved_path = if self.sops_bin.is_absolute() {
            if !self.sops_bin.exists() {
                return Err(AppError::msg(format!(
                    "sops binary not found: {}",
                    self.sops_bin.display()
                )));
            }
            self.sops_bin.display().to_string()
        } else {
            which::which(&self.sops_bin)
                .map(|p| p.display().to_string())
                .map_err(|_| {
                    AppError::msg(format!(
                        "sops binary \"{}\" not found in PATH; pass --sops-binary with an absolute path",
                        self.sops_bin.display()
                    ))
                })?
        };
        log(&format!("Using sops at {resolved_path}"));
        Ok(())
    }

    pub fn download_repo(&self) -> Result<(CleanupPath, PathBuf, String), AppError> {
        self.download_repo_at_revision(None)
    }

    pub fn download_repo_at_revision(
        &self,
        revision: Option<&str>,
    ) -> Result<(CleanupPath, PathBuf, String), AppError> {
        let temp_dir = self.create_temp_dir("reconcile.tmp")?;
        let archive = temp_dir.path().join("archive.tar.gz");
        let token = self.gitlab_token.trim();
        if token.is_empty() {
            return Err(AppError::msg("GitLab access token is empty"));
        }
        log(&format!(
            "Downloading desired state archive{} ...",
            revision.map(|rev| format!(" at {rev}")).unwrap_or_default()
        ));
        download_file(
            &revision
                .map(|rev| self.gitlab.archive_url_for_revision(rev))
                .unwrap_or_else(|| self.gitlab.archive_url()),
            &archive,
            Some((self.gitlab_auth_header.as_str(), token)),
            None,
        )?;

        let revision = detect_revision(&archive)?;
        log(&format!("Fetched desired revision {revision}"));
        extract_archive(&archive, temp_dir.path())?;

        let host_dir = temp_dir.path().join(&self.subfolder);
        if !host_dir.is_dir() {
            return Err(AppError::msg(format!(
                "No configuration directory named {} in archive",
                self.subfolder
            )));
        }
        Ok((temp_dir, host_dir, revision))
    }

    pub fn create_temp_dir(&self, prefix: &str) -> Result<CleanupPath, AppError> {
        let mut attempt = 0;
        loop {
            let suffix = unique_suffix().unwrap_or_else(|| rand_suffix(8));
            let candidate = self.home.join(format!(".{prefix}.{suffix}.{attempt}"));
            if !candidate.exists() {
                fs::create_dir(&candidate)?;
                return Ok(CleanupPath::new(candidate));
            }
            attempt += 1;
        }
    }

    pub fn stage_config(&self, desired: &Path) -> Result<CleanupPath, AppError> {
        let staging = self.create_temp_dir("reconcile.stage")?;
        copy_dir_recursive(desired, staging.path())?;
        Ok(staging)
    }

    pub fn read_current_hash(&self) -> Result<Option<String>, AppError> {
        let hash_file = self.state_dir.join(".config-sha256");
        if hash_file.exists() {
            Ok(Some(fs::read_to_string(hash_file)?.trim().to_string()))
        } else {
            Ok(None)
        }
    }

    pub fn load_running_manifest(&self) -> Option<Result<DeploymentManifest, AppError>> {
        if self.state_dir.join(STOW_DEFINITION_FILE).exists() {
            Some(load_manifest(&self.state_dir, Some(&self.state_dir)))
        } else {
            None
        }
    }

    pub fn load_snapshot_manifests_excluding(
        &self,
        keep_hashes: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<DeploymentManifest>, AppError> {
        let mut manifests = Vec::new();
        if !self.snapshot_root.exists() {
            return Ok(manifests);
        }
        for entry in fs::read_dir(&self.snapshot_root)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let hash = entry.file_name().to_string_lossy().to_string();
            if keep_hashes.contains(&hash) || !path.join(STOW_DEFINITION_FILE).exists() {
                continue;
            }
            match load_manifest(&path, Some(&path)) {
                Ok(manifest) => manifests.push(manifest),
                Err(err) => log(&format!(
                    "Skipping snapshot {} during image prune: {err}",
                    path.display()
                )),
            }
        }
        Ok(manifests)
    }

    pub fn rotate_state_dirs(&self, staging: &mut CleanupPath, hash: &str) -> Result<(), AppError> {
        let snapshot_dir = self.snapshot_dir(hash);
        if snapshot_dir.exists() {
            remove_path_any(staging.path())?;
        } else {
            fs::rename(staging.path(), &snapshot_dir)?;
        }
        staging.disarm();
        self.promote_snapshot(&snapshot_dir)?;
        Ok(())
    }

    pub fn write_metadata(
        &self,
        revision: &str,
        hash: &str,
        deployment_name: &str,
    ) -> Result<(), AppError> {
        fs::write(self.state_dir.join(".git-revision"), revision)?;
        fs::write(self.state_dir.join(".config-sha256"), hash)?;
        fs::write(self.state_dir.join(".deployment-name"), deployment_name)?;
        let rendered_manifest = fs::read_to_string(self.state_dir.join(STOW_DEFINITION_FILE))?;
        fs::write(
            self.state_dir.join(".stow-rendered-manifest.yaml"),
            rendered_manifest,
        )?;
        let snapshot = SnapshotMetadata {
            deployment: deployment_name.to_string(),
            revision: revision.to_string(),
            hash: hash.to_string(),
        };
        fs::write(
            self.state_dir.join(".stow-snapshot.json"),
            serde_json::to_vec_pretty(&snapshot).map_err(|err| {
                AppError::msg(format!("failed to serialize snapshot metadata: {err}"))
            })?,
        )?;
        Ok(())
    }

    pub fn cleanup_previous_dir(&self) -> Result<(), AppError> {
        if self.previous_dir.exists() {
            remove_path_any(&self.previous_dir)?;
        }
        Ok(())
    }

    pub fn restore_previous_state(&self) -> Result<(), AppError> {
        if !self.previous_dir.exists() {
            return Err(AppError::msg(
                "No previous deployment available for rollback",
            ));
        }
        if self.state_dir.exists() {
            remove_path_any(&self.state_dir)?;
        }
        fs::rename(&self.previous_dir, &self.state_dir)?;
        Ok(())
    }

    pub fn read_state_revision(&self) -> Result<String, AppError> {
        Ok(fs::read_to_string(self.state_dir.join(".git-revision"))?
            .trim()
            .to_string())
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn read_current_snapshot_metadata(&self) -> Result<Option<SnapshotMetadata>, AppError> {
        let path = self.state_dir.join(".stow-snapshot.json");
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(path)?;
        let snapshot = serde_json::from_str(&contents)
            .map_err(|err| AppError::msg(format!("failed to parse snapshot metadata: {err}")))?;
        Ok(Some(snapshot))
    }

    pub fn current_snapshot_path(&self) -> Result<Option<PathBuf>, AppError> {
        match fs::read_link(&self.state_dir) {
            Ok(path) => Ok(Some(path)),
            Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {
                Ok(Some(self.state_dir.clone()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn snapshot_dir(&self, hash: &str) -> PathBuf {
        self.snapshot_root.join(hash)
    }

    fn promote_snapshot(&self, snapshot_dir: &Path) -> Result<(), AppError> {
        if self.previous_dir.exists() {
            remove_path_any(&self.previous_dir)?;
        }
        if self.state_dir.exists() {
            fs::rename(&self.state_dir, &self.previous_dir)?;
        }
        create_or_replace_symlink(snapshot_dir, &self.state_dir)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub deployment: String,
    pub revision: String,
    pub hash: String,
}

fn create_or_replace_symlink(target: &Path, link: &Path) -> Result<(), AppError> {
    let temp_link = link.with_extension("next");
    if temp_link.exists() {
        remove_path_any(&temp_link)?;
    }
    symlink(target, &temp_link)?;
    fs::rename(temp_link, link)?;
    Ok(())
}

fn remove_path_any(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || file_type.is_file() {
                fs::remove_file(path)?;
            } else if file_type.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::Context;
    use crate::cli::ReconcileOptions;
    use crate::fs_utils::CleanupPath;
    use crate::gitlab::GitLabConfig;
    use crate::test_support::TempDir;
    use std::fs;
    use std::path::PathBuf;

    fn context(home: &TempDir) -> Context {
        Context::new(
            home.path().to_path_buf(),
            GitLabConfig {
                base_url: "https://git.example/api/v4".to_string(),
                project: "team/deployments".to_string(),
            },
            "host-1".to_string(),
            "token".to_string(),
            "PRIVATE-TOKEN".to_string(),
            ReconcileOptions {
                keys_file: home.path().join("keys.txt"),
                sops_binary: None,
                dry_run: false,
                plan_json: false,
            },
        )
    }

    #[test]
    fn context_derives_state_layout_from_home() {
        let home = TempDir::new("state-ctx");
        let ctx = context(&home);
        assert_eq!(ctx.data_dir, home.path().join(".stow"));
        assert_eq!(ctx.snapshot_root, home.path().join(".stow/snapshots"));
        assert_eq!(ctx.state_dir, home.path().join("running-config"));
        assert_eq!(
            ctx.previous_dir,
            home.path().join("running-config.previous")
        );
        assert_eq!(ctx.lock_dir, home.path().join(".stow/lock"));
        assert_eq!(ctx.sops_bin, PathBuf::from("sops"));
    }

    #[test]
    fn current_hash_reads_trimmed_file_or_none() {
        let home = TempDir::new("state-hash");
        let ctx = context(&home);
        assert_eq!(ctx.read_current_hash().unwrap(), None);

        fs::create_dir_all(&ctx.state_dir).unwrap();
        fs::write(ctx.state_dir.join(".config-sha256"), "  abc123\n").unwrap();
        assert_eq!(ctx.read_current_hash().unwrap().as_deref(), Some("abc123"));
    }

    #[test]
    fn metadata_roundtrip_through_state_dir() {
        let home = TempDir::new("state-meta");
        let ctx = context(&home);
        fs::create_dir_all(&ctx.state_dir).unwrap();
        fs::write(
            ctx.state_dir.join("stow.yaml"),
            "deployment:\n  name: demo\n",
        )
        .unwrap();

        ctx.write_metadata("rev123", "hash456", "demo").unwrap();

        assert_eq!(ctx.read_state_revision().unwrap(), "rev123");
        assert_eq!(ctx.read_current_hash().unwrap().as_deref(), Some("hash456"));
        let snapshot = ctx.read_current_snapshot_metadata().unwrap().unwrap();
        assert_eq!(snapshot.deployment, "demo");
        assert_eq!(snapshot.revision, "rev123");
        assert_eq!(snapshot.hash, "hash456");
        assert_eq!(
            fs::read_to_string(ctx.state_dir.join(".stow-rendered-manifest.yaml")).unwrap(),
            "deployment:\n  name: demo\n"
        );
    }

    #[test]
    fn snapshot_metadata_is_none_without_file_and_errors_on_garbage() {
        let home = TempDir::new("state-meta-missing");
        let ctx = context(&home);
        assert!(ctx.read_current_snapshot_metadata().unwrap().is_none());

        fs::create_dir_all(&ctx.state_dir).unwrap();
        fs::write(ctx.state_dir.join(".stow-snapshot.json"), "not json").unwrap();
        assert!(ctx.read_current_snapshot_metadata().is_err());
    }

    #[test]
    fn rotate_state_dirs_promotes_staging_to_hash_addressed_snapshot() {
        let home = TempDir::new("state-rotate");
        let ctx = context(&home);
        fs::create_dir_all(&ctx.snapshot_root).unwrap();

        let staging_path = home.path().join("staging-1");
        fs::create_dir_all(&staging_path).unwrap();
        fs::write(staging_path.join("stow.yaml"), "v1").unwrap();
        let mut staging = CleanupPath::new(staging_path);

        ctx.rotate_state_dirs(&mut staging, "hash-1").unwrap();

        // staging moved into snapshots/<hash> and running-config points at it
        let snapshot_dir = ctx.snapshot_root.join("hash-1");
        assert!(snapshot_dir.is_dir());
        assert_eq!(fs::read_link(&ctx.state_dir).unwrap(), snapshot_dir);
        assert_eq!(
            fs::read_to_string(ctx.state_dir.join("stow.yaml")).unwrap(),
            "v1"
        );
        assert!(!ctx.previous_dir.exists());
        assert_eq!(ctx.current_snapshot_path().unwrap(), Some(snapshot_dir));
    }

    #[test]
    fn rotating_again_moves_current_state_to_previous() {
        let home = TempDir::new("state-rotate2");
        let ctx = context(&home);
        fs::create_dir_all(&ctx.snapshot_root).unwrap();

        for (idx, hash) in ["hash-1", "hash-2"].iter().enumerate() {
            let staging_path = home.path().join(format!("staging-{idx}"));
            fs::create_dir_all(&staging_path).unwrap();
            fs::write(staging_path.join("stow.yaml"), *hash).unwrap();
            let mut staging = CleanupPath::new(staging_path);
            ctx.rotate_state_dirs(&mut staging, hash).unwrap();
        }

        assert_eq!(
            fs::read_link(&ctx.state_dir).unwrap(),
            ctx.snapshot_root.join("hash-2")
        );
        // previous is the old symlink, still resolving to the first snapshot
        assert_eq!(
            fs::read_to_string(ctx.previous_dir.join("stow.yaml")).unwrap(),
            "hash-1"
        );

        ctx.cleanup_previous_dir().unwrap();
        assert!(!ctx.previous_dir.exists());
    }

    #[test]
    fn rotate_discards_staging_when_snapshot_already_exists() {
        let home = TempDir::new("state-rotate-dup");
        let ctx = context(&home);
        let snapshot_dir = ctx.snapshot_root.join("hash-1");
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(snapshot_dir.join("stow.yaml"), "original").unwrap();

        let staging_path = home.path().join("staging");
        fs::create_dir_all(&staging_path).unwrap();
        fs::write(staging_path.join("stow.yaml"), "duplicate").unwrap();
        let mut staging = CleanupPath::new(staging_path.clone());

        ctx.rotate_state_dirs(&mut staging, "hash-1").unwrap();

        assert!(!staging_path.exists());
        assert_eq!(
            fs::read_to_string(ctx.state_dir.join("stow.yaml")).unwrap(),
            "original"
        );
    }

    #[test]
    fn restore_previous_state_requires_a_previous_dir() {
        let home = TempDir::new("state-restore");
        let ctx = context(&home);
        assert!(ctx.restore_previous_state().is_err());

        fs::create_dir_all(&ctx.previous_dir).unwrap();
        fs::write(ctx.previous_dir.join("stow.yaml"), "old").unwrap();
        fs::create_dir_all(&ctx.state_dir).unwrap();
        fs::write(ctx.state_dir.join("stow.yaml"), "new").unwrap();

        ctx.restore_previous_state().unwrap();
        assert!(!ctx.previous_dir.exists());
        assert_eq!(
            fs::read_to_string(ctx.state_dir.join("stow.yaml")).unwrap(),
            "old"
        );
    }

    #[test]
    fn current_snapshot_path_handles_plain_dir_and_missing_state() {
        let home = TempDir::new("state-snap-path");
        let ctx = context(&home);
        assert_eq!(ctx.current_snapshot_path().unwrap(), None);

        fs::create_dir_all(&ctx.state_dir).unwrap();
        assert_eq!(
            ctx.current_snapshot_path().unwrap(),
            Some(ctx.state_dir.clone())
        );
    }

    #[test]
    fn temp_dirs_are_unique_hidden_and_cleaned_up() {
        let home = TempDir::new("state-temp");
        let ctx = context(&home);
        let first = ctx.create_temp_dir("reconcile.tmp").unwrap();
        let second = ctx.create_temp_dir("reconcile.tmp").unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        assert!(first
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".reconcile.tmp."));

        let kept = first.path().to_path_buf();
        drop(first);
        assert!(!kept.exists());
    }
}
