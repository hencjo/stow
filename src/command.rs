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
    let mut request = ureq::get(url);
    if let Some((name, value)) = header {
        request = request.set(name, value);
    }
    let response = request.call().map_err(|err| map_download_error(err, url))?;
    let mut reader = response.into_reader();
    let mut file = fs::File::create(dest)?;
    std::io::copy(&mut reader, &mut file).map_err(|err| {
        AppError::msg(format!(
            "failed to download {url} to {}: {err}",
            dest.display()
        ))
    })?;
    if let Some(mode) = mode {
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(mode);
        fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

fn map_download_error(err: ureq::Error, url: &str) -> AppError {
    match err {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            AppError::msg(format!("download failed for {url}: HTTP {code}: {body}"))
        }
        ureq::Error::Transport(transport) => AppError::msg(format!(
            "download failed for {url}: transport error: {transport}"
        )),
    }
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
    use super::{
        capture_command, detect_revision, download_file, ensure_command_exists, run_command,
    };
    use crate::test_support::TempDir;
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;

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
    fn download_file_sends_header_and_writes_file_with_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/archive.tar.gz", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            tx.send(request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload",
                )
                .unwrap();
        });

        let dir = TempDir::new("cmd-download");
        let dest = dir.path().join("archive.tar.gz");
        download_file(
            &url,
            &dest,
            Some(("PRIVATE-TOKEN", "secret-token")),
            Some(0o600),
        )
        .unwrap();
        server.join().unwrap();

        let request = rx.recv().unwrap().to_ascii_lowercase();
        assert!(request.contains("private-token: secret-token"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "payload");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
