use crate::app_error::AppError;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct CleanupPath {
    path: Option<PathBuf>,
}

impl CleanupPath {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("cleanup path moved but still borrowed")
    }

    pub fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    pub fn acquire(path: &Path) -> Result<Self, AppError> {
        if path.exists() {
            return Err(AppError::msg(
                "Another reconcile run appears to be in progress (lock exists)",
            ));
        }
        fs::create_dir(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), AppError> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    let src_meta = fs::symlink_metadata(src)?;
    let dir_perms = src_meta.permissions();
    fs::set_permissions(dst, dir_perms)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if ty.is_symlink() {
            let link_target = fs::read_link(&path)?;
            symlink(&link_target, &target)?;
        } else {
            fs::copy(&path, &target)?;
            let metadata = fs::metadata(&path)?;
            let mut perms = metadata.permissions();
            perms.set_mode(metadata.permissions().mode());
            fs::set_permissions(&target, perms)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{copy_dir_recursive, CleanupPath, DirLock};
    use crate::test_support::TempDir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cleanup_path_removes_directory_on_drop() {
        let root = TempDir::new("fsutils-cleanup");
        let dir = root.path().join("scratch");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file"), "x").unwrap();

        let guard = CleanupPath::new(dir.clone());
        assert_eq!(guard.path(), dir.as_path());
        drop(guard);
        assert!(!dir.exists());
    }

    #[test]
    fn disarmed_cleanup_path_keeps_directory() {
        let root = TempDir::new("fsutils-disarm");
        let dir = root.path().join("keep");
        fs::create_dir(&dir).unwrap();

        let mut guard = CleanupPath::new(dir.clone());
        guard.disarm();
        drop(guard);
        assert!(dir.exists());
    }

    #[test]
    fn dir_lock_provides_mutual_exclusion_and_releases_on_drop() {
        let root = TempDir::new("fsutils-lock");
        let lock_path = root.path().join("lock");

        let lock = DirLock::acquire(&lock_path).unwrap();
        assert!(lock_path.is_dir());
        assert!(DirLock::acquire(&lock_path).is_err());

        drop(lock);
        assert!(!lock_path.exists());
        let _relock = DirLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files_symlinks_and_modes() {
        let src = TempDir::new("fsutils-copy-src");
        src.write("a.txt", "alpha");
        src.write("nested/b.txt", "beta");
        let secret = src.write("secret.txt", "shh");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("a.txt", src.path().join("link")).unwrap();

        let dst_root = TempDir::new("fsutils-copy-dst");
        let dst = dst_root.path().join("out");
        copy_dir_recursive(src.path(), &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "beta"
        );
        assert_eq!(
            fs::metadata(dst.join("secret.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let link = dst.join("link");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            std::path::PathBuf::from("a.txt")
        );
    }
}
