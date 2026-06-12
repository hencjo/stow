use crate::app_error::AppError;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub fn run_command(mut command: Command) -> Result<(), AppError> {
    let debug = format!("{command:?}");
    let status = command
        .status()
        .map_err(|err| AppError::msg(format!("failed to run {debug}: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::msg(format!(
            "Command {debug} failed with {status}"
        )))
    }
}

pub fn capture_command(cmd: &str, args: &[&OsStr]) -> Result<String, AppError> {
    let output = Command::new(cmd).args(args).output()?;
    if !output.status.success() {
        return Err(AppError::msg(format!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn ensure_command_exists(cmd: &str) -> Result<(), AppError> {
    let status = Command::new("bash")
        .arg("-lc")
        .arg(format!("command -v {cmd} >/dev/null"))
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::msg(format!("{cmd} command not found in PATH")))
    }
}

pub fn download_file(
    url: &str,
    dest: &Path,
    header: Option<(&str, &str)>,
    mode: Option<u32>,
) -> Result<(), AppError> {
    let mut cmd = Command::new("curl");
    cmd.arg("--silent")
        .arg("--show-error")
        .arg("--fail")
        .arg("--location");
    if let Some((name, value)) = header {
        cmd.arg("--header").arg(format!("{name}: {value}"));
    }
    cmd.arg(url).arg("--output").arg(dest);
    let status = cmd.status()?;
    if !status.success() {
        return Err(AppError::msg(format!("curl download failed for {url}")));
    }
    if let Some(mode) = mode {
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(mode);
        fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

pub fn extract_archive(archive: &Path, target: &Path) -> Result<(), AppError> {
    let mut cmd = Command::new("tar");
    cmd.arg("xfz")
        .arg(archive)
        .arg("-C")
        .arg(target)
        .arg("--strip-components=1");
    run_command(cmd)
}

pub fn detect_revision(archive: &Path) -> Result<String, AppError> {
    let output = capture_command("tar", &[OsStr::new("tzf"), archive.as_os_str()])?;
    let first = output.lines().next().unwrap_or("").trim().to_string();
    if let Some(hash) = first
        .split('-')
        .last()
        .and_then(|segment| segment.strip_suffix('/'))
    {
        Ok(hash.to_string())
    } else {
        Ok("unknown".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{capture_command, detect_revision, ensure_command_exists, run_command};
    use crate::test_support::TempDir;
    use std::ffi::OsStr;
    use std::process::Command;

    fn make_archive(dir: &TempDir, top_level: &str) -> std::path::PathBuf {
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join(top_level)).unwrap();
        std::fs::write(tree.join(top_level).join("stow.yaml"), "x").unwrap();
        let archive = dir.path().join("archive.tar.gz");
        let status = Command::new("tar")
            .arg("czf")
            .arg(&archive)
            .arg("-C")
            .arg(&tree)
            .arg(top_level)
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    #[test]
    fn run_command_reports_exit_status() {
        run_command(Command::new("true")).unwrap();
        assert!(run_command(Command::new("false")).is_err());
    }

    #[test]
    fn capture_command_returns_stdout_or_fails() {
        let output = capture_command("echo", &[OsStr::new("hello")]).unwrap();
        assert_eq!(output, "hello\n");
        assert!(capture_command("false", &[]).is_err());
    }

    #[test]
    fn command_existence_check_uses_path() {
        ensure_command_exists("tar").unwrap();
        assert!(ensure_command_exists("definitely-not-a-command-xyz").is_err());
    }

    #[test]
    fn revision_detection_takes_last_dash_segment_of_top_level_dir() {
        let dir = TempDir::new("cmd-rev");
        let archive = make_archive(&dir, "deployments-main-1d4c74beff");
        assert_eq!(detect_revision(&archive).unwrap(), "1d4c74beff");
    }

    #[test]
    fn revision_detection_without_dashes_returns_dir_name() {
        let dir = TempDir::new("cmd-rev-plain");
        let archive = make_archive(&dir, "plain");
        assert_eq!(detect_revision(&archive).unwrap(), "plain");
    }
}
