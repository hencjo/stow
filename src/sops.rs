use crate::app_error::AppError;
use crate::command::run_command;
use crate::state::Context;
use crate::util::log;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn decrypt_config(
    ctx: &Context,
    dir: &Path,
    temp_root: &Path,
) -> Result<BTreeSet<PathBuf>, AppError> {
    let encrypted_files = find_encrypted_files(dir)?;
    if encrypted_files.is_empty() {
        return Ok(BTreeSet::new());
    }
    let sops_config = temp_root.join(".sops.yaml");
    if !sops_config.exists() {
        return Err(AppError::msg(format!(
            "Missing .sops.yaml in repo root (expected at {})",
            sops_config.display()
        )));
    }
    let mut secret_files = BTreeSet::new();
    for file in encrypted_files {
        log(&format!("Decrypting {}", file.display()));
        let rel = file
            .strip_prefix(dir)
            .map_err(|err| AppError::msg(format!("failed to compute secret relative path: {err}")))?
            .to_path_buf();
        let mut cmd = Command::new(&ctx.sops_bin);
        cmd.env("SOPS_AGE_KEY_FILE", &ctx.age_key_file)
            .arg("--config")
            .arg(&sops_config)
            .arg("--in-place")
            .arg("--decrypt")
            .arg(&file);
        run_command(cmd)?;
        secret_files.insert(rel);
    }
    Ok(secret_files)
}

fn find_encrypted_files(root: &Path) -> Result<Vec<PathBuf>, AppError> {
    let mut result = Vec::new();
    let mut stack = VecDeque::new();
    stack.push_back(root.to_path_buf());
    while let Some(dir) = stack.pop_back() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push_back(path);
            } else if path.is_file() && file_contains(&path, b"ENC[AES")? {
                result.push(path);
            }
        }
    }
    Ok(result)
}

fn file_contains(path: &Path, needle: &[u8]) -> Result<bool, AppError> {
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let mut overlap = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(false);
        }
        let mut combined = overlap;
        combined.extend_from_slice(&buffer[..read]);
        if memmem(&combined, needle).is_some() {
            return Ok(true);
        }
        let keep = needle.len().saturating_sub(1);
        overlap = combined.split_off(combined.len().saturating_sub(keep));
    }
}

fn memmem(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{file_contains, find_encrypted_files, memmem};
    use crate::test_support::TempDir;

    #[test]
    fn memmem_finds_needle_position() {
        assert_eq!(memmem(b"hello world", b"world"), Some(6));
        assert_eq!(memmem(b"hello", b"x"), None);
        assert_eq!(memmem(b"hello", b""), Some(0));
        assert_eq!(memmem(b"", b"x"), None);
    }

    #[test]
    fn file_contains_detects_needle_across_buffer_boundary() {
        let dir = TempDir::new("sops-contains");
        // place the marker right around the 8192-byte read boundary
        let mut content = "x".repeat(8190);
        content.push_str("ENC[AES256_GCM,data:...]");
        let path = dir.write("boundary.yaml", &content);
        assert!(file_contains(&path, b"ENC[AES").unwrap());

        let clean = dir.write("clean.yaml", "key: value\n");
        assert!(!file_contains(&clean, b"ENC[AES").unwrap());
    }

    #[test]
    fn encrypted_file_discovery_walks_nested_directories() {
        let dir = TempDir::new("sops-find");
        dir.write("plain.yaml", "key: value\n");
        let secret = dir.write(
            "nested/deep/secret.yaml",
            "token: ENC[AES256_GCM,data:abc]\n",
        );

        let found = find_encrypted_files(dir.path()).unwrap();
        assert_eq!(found, vec![secret]);
    }
}
