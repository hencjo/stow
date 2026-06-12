use crate::app_error::AppError;
use crate::manifest::STOW_DEFINITION_FILE;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentHashes {
    pub manifest_hash: String,
    pub config_hash: String,
    pub secrets_hash: String,
    pub deployment_hash: String,
}

pub fn compute_deployment_hashes(
    dir: &Path,
    secret_files: &BTreeSet<PathBuf>,
) -> Result<DeploymentHashes, AppError> {
    let manifest_path = dir.join(STOW_DEFINITION_FILE);
    if !manifest_path.exists() {
        return Err(AppError::msg(format!(
            "manifest file missing for hashing: {}",
            manifest_path.display()
        )));
    }

    let manifest_hash = hash_file(&manifest_path, Path::new(STOW_DEFINITION_FILE))?;
    let config_hash = hash_tree(dir, secret_files, TreeKind::Config)?;
    let secrets_hash = hash_tree(dir, secret_files, TreeKind::Secrets)?;
    let deployment_hash = hash_parts(&manifest_hash, &config_hash, &secrets_hash);

    Ok(DeploymentHashes {
        manifest_hash,
        config_hash,
        secrets_hash,
        deployment_hash,
    })
}

#[derive(Clone, Copy)]
enum TreeKind {
    Config,
    Secrets,
}

fn hash_tree(
    root: &Path,
    secret_files: &BTreeSet<PathBuf>,
    kind: TreeKind,
) -> Result<String, AppError> {
    let mut hasher = Sha256::new();
    walk_and_hash(root, Path::new(""), secret_files, kind, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn walk_and_hash(
    root: &Path,
    relative: &Path,
    secret_files: &BTreeSet<PathBuf>,
    kind: TreeKind,
    hasher: &mut Sha256,
) -> Result<(), AppError> {
    let mut entries: Vec<_> = fs::read_dir(root)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let file_name = entry.file_name();
        let rel = relative.join(&file_name);
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            walk_and_hash(&path, &rel, secret_files, kind, hasher)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if rel == Path::new(STOW_DEFINITION_FILE) {
            continue;
        }
        let is_secret = secret_files.contains(&rel);
        match kind {
            TreeKind::Config if is_secret => continue,
            TreeKind::Secrets if !is_secret => continue,
            _ => {}
        }
        update_hasher_with_file(hasher, &path, &rel)?;
    }
    Ok(())
}

fn hash_file(path: &Path, rel: &Path) -> Result<String, AppError> {
    let mut hasher = Sha256::new();
    update_hasher_with_file(&mut hasher, path, rel)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn update_hasher_with_file(hasher: &mut Sha256, path: &Path, rel: &Path) -> Result<(), AppError> {
    hasher.update(rel.to_string_lossy().as_bytes());
    hasher.update(&[0u8]);
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 131_072];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

fn hash_parts(manifest_hash: &str, config_hash: &str, secrets_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"manifest");
    hasher.update(&[0u8]);
    hasher.update(manifest_hash.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(b"config");
    hasher.update(&[0u8]);
    hasher.update(config_hash.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(b"secrets");
    hasher.update(&[0u8]);
    hasher.update(secrets_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::compute_deployment_hashes;
    use crate::test_support::TempDir;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const MANIFEST: &str = "deployment:\n  name: demo\ncontainers: []\n";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn fixture_tree() -> TempDir {
        let dir = TempDir::new("hashing");
        dir.write("stow.yaml", MANIFEST);
        dir.write("app.conf", "key=value\n");
        dir.write("runtime/secret.env", "TOKEN=s3cr3t\n");
        dir
    }

    fn secrets(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn hashing_fails_without_manifest_file() {
        let dir = TempDir::new("hashing");
        assert!(compute_deployment_hashes(dir.path(), &BTreeSet::new()).is_err());
    }

    #[test]
    fn deployment_hashes_match_golden_values() {
        let dir = fixture_tree();
        let hashes =
            compute_deployment_hashes(dir.path(), &secrets(&["runtime/secret.env"])).unwrap();

        // sha256("stow.yaml" || 0x00 || manifest content)
        assert_eq!(
            hashes.manifest_hash,
            "03af79fe1f78a6e6dc4a2aee57d8bf20ab2e941b38d764411c2651f01a7dc8d9"
        );
        // sha256("app.conf" || 0x00 || contents); stow.yaml and secrets excluded
        assert_eq!(
            hashes.config_hash,
            "d0d782f7367a504d33c6e3664377fc4aa5737666d637df3b138038315c51edab"
        );
        // sha256("runtime/secret.env" || 0x00 || contents)
        assert_eq!(
            hashes.secrets_hash,
            "49d2fbb81c670437626813e96cfcde797914f1e5b77260b50d857d16f95c90b7"
        );
        // sha256("manifest" 0 mh 0 "config" 0 ch 0 "secrets" 0 sh)
        assert_eq!(
            hashes.deployment_hash,
            "26afd0934a76c81a3835f3cd3fa74c73a37b8c705957407d4b40c43f6c7fa61d"
        );
    }

    #[test]
    fn manifest_only_tree_yields_empty_config_and_secrets_hashes() {
        let dir = TempDir::new("hashing");
        dir.write("stow.yaml", MANIFEST);
        let hashes = compute_deployment_hashes(dir.path(), &BTreeSet::new()).unwrap();
        assert_eq!(hashes.config_hash, EMPTY_SHA256);
        assert_eq!(hashes.secrets_hash, EMPTY_SHA256);
    }

    #[test]
    fn hashing_is_deterministic_across_identical_trees() {
        let secret_set = secrets(&["runtime/secret.env"]);
        let first = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();
        let second = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();
        assert_eq!(first.deployment_hash, second.deployment_hash);
    }

    #[test]
    fn secret_file_content_only_affects_secrets_hash() {
        let secret_set = secrets(&["runtime/secret.env"]);
        let base = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();

        let changed = fixture_tree();
        changed.write("runtime/secret.env", "TOKEN=changed\n");
        let after = compute_deployment_hashes(changed.path(), &secret_set).unwrap();

        assert_eq!(base.manifest_hash, after.manifest_hash);
        assert_eq!(base.config_hash, after.config_hash);
        assert_ne!(base.secrets_hash, after.secrets_hash);
        assert_ne!(base.deployment_hash, after.deployment_hash);
    }

    #[test]
    fn config_file_content_only_affects_config_hash() {
        let secret_set = secrets(&["runtime/secret.env"]);
        let base = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();

        let changed = fixture_tree();
        changed.write("app.conf", "key=other\n");
        let after = compute_deployment_hashes(changed.path(), &secret_set).unwrap();

        assert_eq!(base.manifest_hash, after.manifest_hash);
        assert_ne!(base.config_hash, after.config_hash);
        assert_eq!(base.secrets_hash, after.secrets_hash);
        assert_ne!(base.deployment_hash, after.deployment_hash);
    }

    #[test]
    fn manifest_content_affects_manifest_hash_but_not_config_hash() {
        let secret_set = secrets(&["runtime/secret.env"]);
        let base = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();

        let changed = fixture_tree();
        changed.write("stow.yaml", "deployment:\n  name: other\ncontainers: []\n");
        let after = compute_deployment_hashes(changed.path(), &secret_set).unwrap();

        assert_ne!(base.manifest_hash, after.manifest_hash);
        assert_eq!(base.config_hash, after.config_hash);
        assert_ne!(base.deployment_hash, after.deployment_hash);
    }

    #[test]
    fn renaming_a_file_changes_the_config_hash() {
        let secret_set = secrets(&["runtime/secret.env"]);
        let base = compute_deployment_hashes(fixture_tree().path(), &secret_set).unwrap();

        let renamed = fixture_tree();
        std::fs::rename(
            renamed.path().join("app.conf"),
            renamed.path().join("renamed.conf"),
        )
        .unwrap();
        let after = compute_deployment_hashes(renamed.path(), &secret_set).unwrap();

        assert_ne!(base.config_hash, after.config_hash);
    }
}
