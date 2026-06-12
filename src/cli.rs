use crate::app_error::AppError;
use crate::gitlab::GitLabConfig;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub struct CliOptions {
    pub gitlab: Option<GitLabConfig>,
    pub subfolder: String,
    pub gitlab_token: String,
    pub gitlab_auth_header: String,
    pub mode: OperationMode,
}

pub enum OperationMode {
    Suggest(SuggestOptions),
    Reconcile(ReconcileOptions),
    Daemon(DaemonOptions),
    InstallSystemd(InstallSystemdOptions),
}

#[derive(Clone)]
pub struct ReconcileOptions {
    pub keys_file: PathBuf,
    pub sops_binary: Option<PathBuf>,
    pub dry_run: bool,
    pub plan_json: bool,
}

#[derive(Clone)]
pub struct DaemonOptions {
    pub config_path: PathBuf,
    pub listen: String,
    pub tls_crt: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub reconcile: ReconcileOptions,
}

pub struct InstallSystemdOptions {
    pub config_path: PathBuf,
}

pub struct SuggestOptions {
    pub image: String,
    pub digest: Option<String>,
    pub changelog_file: Option<String>,
    pub container: Option<String>,
    pub assign: Vec<AssignAttempt>,
}

#[derive(Clone, Debug)]
pub enum AssignAttempt {
    GitLabUserId,
    Id(u64),
    User(String),
}

impl CliOptions {
    pub fn parse(_home: &Path, default_subfolder: &str) -> Result<Self, AppError> {
        let mut gitlab_base: Option<String> = None;
        let mut gitlab_project: Option<String> = None;
        let mut gitlab_token_config: Option<String> = None;
        let mut keys_file = None;
        let mut subfolder = default_subfolder.to_string();
        let mut subfolder_set = false;
        let mut sops_binary = None;
        let mut dry_run = false;
        let mut plan_json = false;
        let mut daemon_listen = "0.0.0.0:17403".to_string();
        let mut tls_crt = None;
        let mut tls_key = None;
        let mut suggest_image = None;
        let mut suggest_digest = None;
        let mut changelog_file = None;
        let mut suggest_container = None;
        let mut suggest_assign = Vec::new();
        let mut args = env::args().skip(1).collect::<Vec<_>>();

        if args.iter().any(|a| a == "--help" || a == "-h") {
            print_usage();
            std::process::exit(0);
        }

        if args.is_empty() {
            eprintln!("error: MODE must be provided.");
            print_usage();
            return Err(AppError::msg("mode is required"));
        }

        let mode_arg = args.remove(0);
        if mode_arg.starts_with('-') {
            eprintln!("error: MODE must be provided before options.");
            print_usage();
            return Err(AppError::msg("mode is required"));
        }
        let mode_kind = match parse_mode(&mode_arg) {
            Ok(mode) => mode,
            Err(err) => {
                print_usage();
                return Err(err);
            }
        };

        let config_path = config_path_from_args(&args, mode_kind)?;
        if matches!(mode_kind, ModeKind::Suggest) && config_path.is_some() {
            return Err(AppError::msg(
                "suggest-image mode does not use --config; use GitLab CI env for GitLab fields and CLI args for the bump",
            ));
        }
        if matches!(
            mode_kind,
            ModeKind::Reconcile | ModeKind::Daemon | ModeKind::InstallSystemd
        ) && config_path.is_none()
        {
            return Err(AppError::msg(format!(
                "--config is required in {} mode",
                mode_kind.name()
            )));
        }
        if let Some(config_path) = config_path.as_deref() {
            let config = load_config(config_path)?;
            gitlab_base = non_empty(config.gitlab_base);
            gitlab_project = non_empty(config.project);
            gitlab_token_config = non_empty(config.gitlab_token);
            if let Some(value) = non_empty(config.subfolder) {
                subfolder = value;
                subfolder_set = true;
            }
            keys_file = config.keys;
            sops_binary = config.sops_binary;
            if let Some(value) = non_empty(config.listen) {
                daemon_listen = value;
            }
            tls_crt = config.tls_crt;
            tls_key = config.tls_key;
        }

        let mut idx = 0;
        while idx < args.len() {
            let arg = &args[idx];
            match arg.as_str() {
                "--config" => {
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--config requires a path"))?;
                    if value.trim().is_empty() {
                        return Err(AppError::msg("--config cannot be empty"));
                    }
                }
                "--project" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(match mode_kind {
                            ModeKind::Reconcile | ModeKind::Daemon => {
                                "--project belongs in the config file"
                            }
                            ModeKind::InstallSystemd => {
                                "--project is not used in install-systemd mode"
                            }
                            ModeKind::Suggest => unreachable!(),
                        }));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--project requires a value"))?
                        .clone();
                    if value.trim().is_empty() {
                        return Err(AppError::msg("--project cannot be empty"));
                    }
                    gitlab_project = Some(value);
                }
                "--dry-run" => {
                    if !matches!(mode_kind, ModeKind::Reconcile) {
                        return Err(AppError::msg(
                            "--dry-run is only available in reconcile mode",
                        ));
                    }
                    dry_run = true;
                }
                "--plan-json" => {
                    if !matches!(mode_kind, ModeKind::Reconcile) {
                        return Err(AppError::msg(
                            "--plan-json is only available in reconcile mode",
                        ));
                    }
                    plan_json = true;
                }
                "--subfolder" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(match mode_kind {
                            ModeKind::Reconcile | ModeKind::Daemon => {
                                "--subfolder belongs in the config file"
                            }
                            ModeKind::InstallSystemd => {
                                "--subfolder is not used in install-systemd mode"
                            }
                            ModeKind::Suggest => unreachable!(),
                        }));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--subfolder requires a value"))?
                        .clone();
                    if value.trim().is_empty() {
                        return Err(AppError::msg("--subfolder cannot be empty"));
                    }
                    subfolder = value;
                    subfolder_set = true;
                }
                "--image" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(
                            "--image is only available in suggest-image mode",
                        ));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--image requires a value"))?
                        .clone();
                    suggest_image = Some(value);
                }
                "--digest" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(
                            "--digest is only available in suggest-image mode",
                        ));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--digest requires a value"))?
                        .clone();
                    suggest_digest = Some(value);
                }
                "--changelog-file" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(
                            "--changelog-file is only available in suggest-image mode",
                        ));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--changelog-file requires a value"))?
                        .clone();
                    if value.trim().is_empty() {
                        return Err(AppError::msg("--changelog-file cannot be empty"));
                    }
                    changelog_file = Some(value);
                }
                "--container" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(
                            "--container is only available in suggest-image mode",
                        ));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--container requires a value"))?
                        .clone();
                    if value.trim().is_empty() {
                        return Err(AppError::msg("--container cannot be empty"));
                    }
                    suggest_container = Some(value);
                }
                "--assign" => {
                    if !matches!(mode_kind, ModeKind::Suggest) {
                        return Err(AppError::msg(
                            "--assign is only available in suggest-image mode",
                        ));
                    }
                    idx += 1;
                    let value = args
                        .get(idx)
                        .ok_or_else(|| AppError::msg("--assign requires a value"))?;
                    suggest_assign.extend(parse_assign_attempts(value)?);
                }
                _ => {
                    return Err(AppError::msg(format!(
                        "Unknown argument \"{arg}\". Use --help for usage."
                    )));
                }
            }
            idx += 1;
        }

        let needs_gitlab = !matches!(mode_kind, ModeKind::InstallSystemd);
        let gitlab = if needs_gitlab {
            let gitlab_base = if matches!(mode_kind, ModeKind::Suggest) {
                let api_url = non_empty_env("CI_API_V4_URL").ok_or_else(|| {
                    AppError::msg("CI_API_V4_URL is required in suggest-image mode")
                })?;
                println!("Using CI_API_V4_URL: {api_url}");
                api_url
            } else {
                gitlab_base.ok_or_else(|| AppError::msg("gitlabBase is required in config"))?
            };
            let gitlab_project = gitlab_project.ok_or_else(|| {
                AppError::msg(if matches!(mode_kind, ModeKind::Suggest) {
                    "--project is required in suggest-image mode"
                } else {
                    "project is required in config"
                })
            })?;
            Some(GitLabConfig {
                base_url: gitlab_base,
                project: gitlab_project,
            })
        } else {
            None
        };

        let (gitlab_token, gitlab_auth_header) = if needs_gitlab {
            if matches!(mode_kind, ModeKind::Suggest) {
                match env::var("GITLAB_ACCESS_TOKEN") {
                    Ok(value) if !value.trim().is_empty() => {
                        (value.trim().to_string(), "PRIVATE-TOKEN".to_string())
                    }
                    _ => match env::var("CI_JOB_TOKEN") {
                        Ok(token) if !token.trim().is_empty() => {
                            (token.trim().to_string(), "JOB-TOKEN".to_string())
                        }
                        _ => {
                            eprintln!(
                                "error: GITLAB_ACCESS_TOKEN env var is required (CI_JOB_TOKEN not available)."
                            );
                            print_usage();
                            return Err(AppError::msg("GITLAB_ACCESS_TOKEN env var is required"));
                        }
                    },
                }
            } else {
                (
                    gitlab_token_config
                        .ok_or_else(|| AppError::msg("gitlabToken is required in config"))?,
                    "PRIVATE-TOKEN".to_string(),
                )
            }
        } else {
            ("".to_string(), "PRIVATE-TOKEN".to_string())
        };
        if needs_gitlab && gitlab_token.trim().is_empty() {
            return Err(AppError::msg(if matches!(mode_kind, ModeKind::Suggest) {
                "GitLab token cannot be empty"
            } else {
                "gitlabToken is required in config"
            }));
        }
        if matches!(mode_kind, ModeKind::Suggest) {
            println!(
                "[cli] Using {} for GitLab authentication.",
                if gitlab_auth_header == "JOB-TOKEN" {
                    "CI_JOB_TOKEN"
                } else {
                    "GITLAB_ACCESS_TOKEN"
                }
            );
        }

        if matches!(mode_kind, ModeKind::Daemon)
            && (matches!(tls_crt, Some(_)) != matches!(tls_key, Some(_)))
        {
            return Err(AppError::msg(
                "--tls-crt and --tls-key must be provided together",
            ));
        }

        let mode = match mode_kind {
            ModeKind::Suggest => {
                if !subfolder_set {
                    return Err(AppError::msg(
                        "--subfolder is required in suggest-image mode",
                    ));
                }
                let image = suggest_image
                    .ok_or_else(|| AppError::msg("--image is required in suggest-image mode"))?;
                if suggest_container.is_none() {
                    return Err(AppError::msg(
                        "--container is required in suggest-image mode",
                    ));
                }
                OperationMode::Suggest(SuggestOptions {
                    image,
                    digest: suggest_digest,
                    changelog_file,
                    container: suggest_container,
                    assign: suggest_assign,
                })
            }
            ModeKind::Reconcile => {
                let keys_file = keys_file
                    .clone()
                    .ok_or_else(|| AppError::msg("keys is required in config"))?;
                OperationMode::Reconcile(ReconcileOptions {
                    keys_file,
                    sops_binary,
                    dry_run,
                    plan_json,
                })
            }
            ModeKind::Daemon => {
                let config_path = absolute_path(
                    config_path
                        .as_ref()
                        .ok_or_else(|| AppError::msg("--config is required in daemon mode"))?,
                )?;
                OperationMode::Daemon(DaemonOptions {
                    config_path,
                    listen: daemon_listen,
                    tls_crt: Some(tls_crt.clone().ok_or_else(|| {
                        AppError::msg("tlsCrt is required in config for daemon mode")
                    })?),
                    tls_key: Some(tls_key.clone().ok_or_else(|| {
                        AppError::msg("tlsKey is required in config for daemon mode")
                    })?),
                    reconcile: ReconcileOptions {
                        keys_file: keys_file
                            .clone()
                            .ok_or_else(|| AppError::msg("keys is required in config"))?,
                        sops_binary,
                        dry_run: false,
                        plan_json: false,
                    },
                })
            }
            ModeKind::InstallSystemd => {
                let config_path = absolute_path(config_path.as_ref().ok_or_else(|| {
                    AppError::msg("--config is required in install-systemd mode")
                })?)?;
                OperationMode::InstallSystemd(InstallSystemdOptions { config_path })
            }
        };

        Ok(Self {
            gitlab,
            subfolder,
            gitlab_token,
            gitlab_auth_header,
            mode,
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModeConfig {
    gitlab_base: Option<String>,
    project: Option<String>,
    gitlab_token: Option<String>,
    subfolder: Option<String>,
    keys: Option<PathBuf>,
    sops_binary: Option<PathBuf>,
    listen: Option<String>,
    tls_crt: Option<PathBuf>,
    tls_key: Option<PathBuf>,
}

fn config_path_from_args(args: &[String], mode: ModeKind) -> Result<Option<PathBuf>, AppError> {
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == "--config" {
            let value = args
                .get(idx + 1)
                .ok_or_else(|| AppError::msg("--config requires a path"))?;
            if value.trim().is_empty() {
                return Err(AppError::msg("--config cannot be empty"));
            }
            return Ok(Some(PathBuf::from(value)));
        }
        idx += 1;
    }
    let _ = mode;
    Ok(None)
}

fn load_config(path: &Path) -> Result<ModeConfig, AppError> {
    let raw = fs::read_to_string(path)
        .map_err(|err| AppError::msg(format!("failed to read config {}: {err}", path.display())))?;
    serde_yaml::from_str(&raw)
        .map_err(|err| AppError::msg(format!("failed to parse config {}: {err}", path.display())))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn parse_assign_attempts(raw: &str) -> Result<Vec<AssignAttempt>, AppError> {
    let mut attempts = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        if item == "gitlab_user_id" {
            attempts.push(AssignAttempt::GitLabUserId);
        } else if let Some(id) = item.strip_prefix("id:") {
            let id = id.trim();
            if id.is_empty() {
                return Err(AppError::msg("--assign id: requires a value"));
            }
            attempts.push(AssignAttempt::Id(id.parse::<u64>().map_err(|_| {
                AppError::msg("--assign id:<value> must be a numeric GitLab user ID")
            })?));
        } else if let Some(username) = item.strip_prefix("user:") {
            let username = username.trim();
            if username.is_empty() {
                return Err(AppError::msg("--assign user: requires a username"));
            }
            attempts.push(AssignAttempt::User(username.to_string()));
        } else {
            return Err(AppError::msg(format!(
                "Unknown --assign item \"{item}\"; use gitlab_user_id, id:<id>, or user:<username>"
            )));
        }
    }
    if attempts.is_empty() {
        return Err(AppError::msg(
            "--assign requires at least one assignment attempt",
        ));
    }
    Ok(attempts)
}

#[derive(Clone, Copy)]
enum ModeKind {
    Suggest,
    Reconcile,
    Daemon,
    InstallSystemd,
}

impl ModeKind {
    fn name(self) -> &'static str {
        match self {
            ModeKind::Suggest => "suggest-image",
            ModeKind::Reconcile => "reconcile",
            ModeKind::Daemon => "daemon",
            ModeKind::InstallSystemd => "install-systemd",
        }
    }
}

fn parse_mode(value: &str) -> Result<ModeKind, AppError> {
    match value {
        "suggest-image" => Ok(ModeKind::Suggest),
        "reconcile" => Ok(ModeKind::Reconcile),
        "daemon" => Ok(ModeKind::Daemon),
        "install-systemd" => Ok(ModeKind::InstallSystemd),
        other => Err(AppError::msg(format!(
            "Unknown mode \"{}\". Allowed modes: reconcile, suggest-image, daemon, install-systemd",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        config_path_from_args, load_config, non_empty, parse_assign_attempts, parse_mode,
        AssignAttempt, ModeKind,
    };
    use crate::test_support::TempDir;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn mode_parsing_accepts_the_four_modes_only() {
        assert!(matches!(parse_mode("suggest-image"), Ok(ModeKind::Suggest)));
        assert!(matches!(parse_mode("reconcile"), Ok(ModeKind::Reconcile)));
        assert!(matches!(parse_mode("daemon"), Ok(ModeKind::Daemon)));
        assert!(matches!(
            parse_mode("install-systemd"),
            Ok(ModeKind::InstallSystemd)
        ));
        assert!(parse_mode("deploy").is_err());
        assert!(parse_mode("").is_err());
    }

    #[test]
    fn assign_attempts_parse_all_forms_in_order() {
        let attempts = parse_assign_attempts("gitlab_user_id, id:146 ,user:some.username").unwrap();
        assert_eq!(attempts.len(), 3);
        assert!(matches!(attempts[0], AssignAttempt::GitLabUserId));
        assert!(matches!(attempts[1], AssignAttempt::Id(146)));
        assert!(matches!(&attempts[2], AssignAttempt::User(name) if name == "some.username"));
    }

    #[test]
    fn assign_attempts_reject_invalid_items() {
        assert!(parse_assign_attempts("").is_err());
        assert!(parse_assign_attempts(" , ").is_err());
        assert!(parse_assign_attempts("bogus").is_err());
        assert!(parse_assign_attempts("id:").is_err());
        assert!(parse_assign_attempts("id:abc").is_err());
        assert!(parse_assign_attempts("user:").is_err());
    }

    #[test]
    fn non_empty_trims_and_filters_blank_values() {
        assert_eq!(non_empty(Some("  x  ".to_string())).as_deref(), Some("x"));
        assert_eq!(non_empty(Some("   ".to_string())), None);
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn config_path_extraction_from_args() {
        assert_eq!(
            config_path_from_args(&args(&["--config", "/etc/stow.yaml"]), ModeKind::Reconcile)
                .unwrap(),
            Some(PathBuf::from("/etc/stow.yaml"))
        );
        assert_eq!(
            config_path_from_args(&args(&["--dry-run"]), ModeKind::Reconcile).unwrap(),
            None
        );
        assert!(config_path_from_args(&args(&["--config"]), ModeKind::Reconcile).is_err());
        assert!(config_path_from_args(&args(&["--config", "  "]), ModeKind::Reconcile).is_err());
    }

    #[test]
    fn config_file_parses_camel_case_fields() {
        let dir = TempDir::new("cli-config");
        let path = dir.write(
            "stow.yaml",
            "gitlabBase: https://git.example/api/v4\nproject: team/deployments\ngitlabToken: glpat-x\nsubfolder: host-1\nkeys: /root/keys.txt\nsopsBinary: /usr/bin/sops\nlisten: 0.0.0.0:17403\ntlsCrt: /etc/stow/tls.crt\ntlsKey: /etc/stow/tls.key\n",
        );
        let config = load_config(&path).unwrap();
        assert_eq!(
            config.gitlab_base.as_deref(),
            Some("https://git.example/api/v4")
        );
        assert_eq!(config.project.as_deref(), Some("team/deployments"));
        assert_eq!(config.gitlab_token.as_deref(), Some("glpat-x"));
        assert_eq!(config.subfolder.as_deref(), Some("host-1"));
        assert_eq!(config.keys, Some(PathBuf::from("/root/keys.txt")));
        assert_eq!(config.sops_binary, Some(PathBuf::from("/usr/bin/sops")));
        assert_eq!(config.listen.as_deref(), Some("0.0.0.0:17403"));
        assert_eq!(config.tls_crt, Some(PathBuf::from("/etc/stow/tls.crt")));
        assert_eq!(config.tls_key, Some(PathBuf::from("/etc/stow/tls.key")));
    }

    #[test]
    fn config_file_load_fails_on_missing_or_invalid_file() {
        let dir = TempDir::new("cli-config");
        assert!(load_config(&dir.path().join("missing.yaml")).is_err());
        let bad = dir.write("bad.yaml", "gitlabBase: [unclosed\n");
        assert!(load_config(&bad).is_err());
    }
}

fn print_usage() {
    eprintln!(
        "\
Usage: stow MODE [OPTIONS]

Modes (required):
  suggest-image        Suggest bumping the Docker image version
  reconcile            Apply desired state
  daemon               Run webhook-driven reconcile daemon
  install-systemd      Upsert and restart the stow systemd service

suggest-image args:
  --project <NAME>         GitLab project ID or path for deployment repo (required)
  --subfolder <NAME>       Directory inside deployment repo (required)
  --image <REF>            Docker image reference without digest (required)
  --digest <HASH>          SHA256 manifest digest (optional, will resolve if omitted)
  --container <NAME>       Target container (required)
  --assign <ORDER>         Ordered MR assignment attempts: gitlab_user_id,id:<id>,user:<username>
  --changelog-file <PATH> Include added markdown changelog lines between image tags (optional)
suggest-image env:
  CI_API_V4_URL             GitLab API base URL (required)
  GITLAB_ACCESS_TOKEN       GitLab token (preferred)
  CI_JOB_TOKEN              fallback GitLab CI token
  GITLAB_USER_ID            used by --assign gitlab_user_id

reconcile args:
  --config <PATH>           Reconcile config file (required)
  --dry-run                 Compute and print the plan without changing Docker
  --plan-json               Emit reconcile plan as JSON

daemon args:
  --config <PATH>           Daemon config file (required)

install-systemd args:
  --config <PATH>           Daemon config file to run (required)

General:
  -h, --help           Show this help message
",
    );
}
