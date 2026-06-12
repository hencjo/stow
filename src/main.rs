mod app_error;
mod cli;
mod command;
mod daemon;
mod docker;
mod fs_utils;
mod gitlab;
mod hashing;
mod manifest;
mod reconcile;
mod sops;
mod state;
mod suggest;
mod systemd;
#[cfg(test)]
mod test_support;
mod util;

use crate::app_error::AppError;
use crate::cli::{CliOptions, OperationMode};
use crate::command::capture_command;
use crate::daemon::serve;
use crate::reconcile::run_reconcile_once;
use crate::state::Context;
use crate::suggest::suggest;
use crate::systemd::install_systemd_service;
use crate::util::{log, set_umask};
use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;

fn main() {
    set_umask();
    if let Err(err) = run() {
        eprintln!("[stow][error] {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let home = env::var_os("HOME").ok_or_else(|| AppError::msg("$HOME is not set"))?;
    let home = PathBuf::from(home);
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| {
        capture_command("hostname", &[] as &[&OsStr])
            .unwrap_or_else(|_| "unknown-host".to_string())
            .trim()
            .to_string()
    });
    let cli = CliOptions::parse(&home, &hostname)?;
    let CliOptions {
        gitlab,
        subfolder,
        gitlab_token,
        gitlab_auth_header,
        mode,
    } = cli;

    match mode {
        OperationMode::Suggest(suggest_opts) => {
            let gitlab = gitlab.ok_or_else(|| AppError::msg("GitLab config missing"))?;
            suggest(
                &gitlab,
                &subfolder,
                &gitlab_token,
                &gitlab_auth_header,
                &suggest_opts,
            )
        }
        OperationMode::Reconcile(reconcile_opts) => {
            let gitlab = gitlab.ok_or_else(|| AppError::msg("GitLab config missing"))?;
            log(&format!(
                "Resolved options: gitlab-base={}, project={}, keys={}, subfolder={}, sops-binary={}",
                gitlab.trimmed_base(),
                gitlab.project,
                reconcile_opts.keys_file.display(),
                subfolder,
                reconcile_opts
                    .sops_binary
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "sops (PATH)".to_string())
            ));

            let ctx = Context::new(
                home,
                gitlab.clone(),
                subfolder,
                gitlab_token,
                gitlab_auth_header,
                reconcile_opts.clone(),
            );
            ctx.ensure_prereqs()?;
            run_reconcile_once(&ctx, &reconcile_opts)
        }
        OperationMode::Daemon(daemon_opts) => {
            let gitlab = gitlab.ok_or_else(|| AppError::msg("GitLab config missing"))?;
            let reconcile_opts = daemon_opts.reconcile.clone();
            log(&format!(
                "Resolved daemon options: gitlab-base={}, project={}, keys={}, subfolder={}, listen={}, tls={}, sops-binary={}",
                gitlab.trimmed_base(),
                gitlab.project,
                reconcile_opts.keys_file.display(),
                subfolder,
                daemon_opts.listen,
                match (&daemon_opts.tls_crt, &daemon_opts.tls_key) {
                    (Some(crt), Some(key)) => format!("{} {}", crt.display(), key.display()),
                    _ => "disabled".to_string(),
                },
                reconcile_opts
                    .sops_binary
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "sops (PATH)".to_string())
            ));
            let ctx = Context::new(
                home,
                gitlab.clone(),
                subfolder,
                gitlab_token,
                gitlab_auth_header,
                reconcile_opts,
            );
            serve(ctx, daemon_opts)
        }
        OperationMode::InstallSystemd(install_opts) => {
            let _ = gitlab_token;
            install_systemd_service(&install_opts)
        }
    }
}
