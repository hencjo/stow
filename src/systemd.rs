use crate::app_error::AppError;
use crate::cli::InstallSystemdOptions;
use crate::command::run_command;
use crate::util::log;
use std::path::Path;
use std::process::Command;

const UNIT_PATH: &str = "/etc/systemd/system/stow.service";
const SERVICE_NAME: &str = "stow.service";

pub fn install_systemd_service(opts: &InstallSystemdOptions) -> Result<(), AppError> {
    let binary = std::env::current_exe()
        .map_err(|err| AppError::msg(format!("failed to resolve current executable: {err}")))?;
    if !binary.is_absolute() {
        return Err(AppError::msg(format!(
            "current executable path is not absolute: {}",
            binary.display()
        )));
    }

    let unit_body = render_unit(&binary, opts)?;

    std::fs::write(UNIT_PATH, &unit_body)?;

    log(&format!("Wrote {UNIT_PATH}"));

    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", SERVICE_NAME])?;
    run_systemctl(&["restart", SERVICE_NAME])?;

    log("Installed and restarted stow.service.");
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<(), AppError> {
    let mut cmd = Command::new("systemctl");
    cmd.args(args);
    run_command(cmd)
}

fn render_unit(binary: &Path, opts: &InstallSystemdOptions) -> Result<String, AppError> {
    let exec = [
        quote_systemd_arg(binary.to_string_lossy().as_ref())?,
        "daemon".to_string(),
        "--config".to_string(),
        quote_systemd_arg(opts.config_path.to_string_lossy().as_ref())?,
    ];
    Ok(format!(
        "[Unit]\nDescription=stow daemon\nWants=network-online.target\nRequires=docker.service\nAfter=network-online.target docker.service\n\n[Service]\nType=simple\nUser=root\nWorkingDirectory=/root\nEnvironment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\nEnvironment=HOME=/root\nExecStart={}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n",
        exec.join(" ")
    ))
}

fn quote_systemd_arg(value: &str) -> Result<String, AppError> {
    reject_control_chars(value)?;
    if value.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '@' | '+')
    }) {
        return Ok(value.to_string());
    }
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("$$"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    Ok(out)
}

fn reject_control_chars(value: &str) -> Result<(), AppError> {
    if value
        .chars()
        .any(|ch| ch == '\n' || ch == '\r' || ch == '\0')
    {
        return Err(AppError::msg(
            "systemd installer values must not contain newlines or NUL bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::InstallSystemdOptions;
    use std::path::PathBuf;

    #[test]
    fn rendered_unit_uses_config_file_only() {
        let opts = InstallSystemdOptions {
            config_path: PathBuf::from("/etc/stow/daemon.yaml"),
        };
        let unit = render_unit(Path::new("/usr/local/bin/stow"), &opts).unwrap();

        assert!(
            unit.contains("ExecStart=/usr/local/bin/stow daemon --config /etc/stow/daemon.yaml")
        );
        assert!(!unit.contains("--project"));
        assert!(!unit.contains("--subfolder"));
        assert!(!unit.contains("--tls-crt"));
        assert!(!unit.contains("--tls-key"));
        assert!(!unit.contains("--keys"));
        assert!(!unit.contains("--listen"));
        assert!(!unit.contains("EnvironmentFile="));
        assert!(!unit.contains("GITLAB_ACCESS_TOKEN"));
    }

    #[test]
    fn rendered_unit_pins_runtime_environment() {
        let opts = InstallSystemdOptions {
            config_path: PathBuf::from("/etc/stow/daemon.yaml"),
        };
        let unit = render_unit(Path::new("/usr/local/bin/stow"), &opts).unwrap();
        assert!(unit.contains("User=root\n"));
        assert!(unit.contains("Requires=docker.service\n"));
        assert!(unit.contains("After=network-online.target docker.service\n"));
        assert!(unit.contains("Restart=always\n"));
        assert!(unit.contains("Environment=HOME=/root\n"));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn systemd_arg_quoting_passes_safe_values_through() {
        assert_eq!(
            quote_systemd_arg("/usr/local/bin/stow").unwrap(),
            "/usr/local/bin/stow"
        );
        assert_eq!(quote_systemd_arg("a-b_c.d:e@f+g").unwrap(), "a-b_c.d:e@f+g");
    }

    #[test]
    fn systemd_arg_quoting_escapes_specials_and_rejects_control_chars() {
        assert_eq!(quote_systemd_arg("a b").unwrap(), "\"a b\"");
        assert_eq!(quote_systemd_arg("a\"b").unwrap(), "\"a\\\"b\"");
        assert_eq!(quote_systemd_arg("a\\b").unwrap(), "\"a\\\\b\"");
        assert_eq!(quote_systemd_arg("$HOME").unwrap(), "\"$$HOME\"");
        assert!(quote_systemd_arg("a\nb").is_err());
        assert!(quote_systemd_arg("a\rb").is_err());
        assert!(quote_systemd_arg("a\0b").is_err());
    }
}
