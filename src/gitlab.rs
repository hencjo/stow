use crate::app_error::AppError;
use percent_encoding::{percent_encode, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use ureq::{Error as UreqError, Response};

#[derive(Clone)]
pub struct GitLabConfig {
    pub base_url: String,
    pub project: String,
}

fn encode_component(value: &str) -> String {
    percent_encode(value.as_bytes(), NON_ALPHANUMERIC).to_string()
}

impl GitLabConfig {
    pub fn archive_url(&self) -> String {
        format!(
            "{}/projects/{}/repository/archive.tar.gz",
            self.trimmed_base(),
            self.encoded_project()
        )
    }

    pub fn archive_url_for_revision(&self, revision: &str) -> String {
        format!("{}?sha={}", self.archive_url(), encode_component(revision))
    }

    fn encoded_project(&self) -> String {
        encode_component(&self.project)
    }

    pub fn trimmed_base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    fn project_api(&self, suffix: &str) -> String {
        format!("/projects/{}{}", self.encoded_project(), suffix)
    }
}

pub struct GitLabClient<'a> {
    config: &'a GitLabConfig,
    token: &'a str,
    auth_header: &'a str,
}

#[derive(Deserialize)]
pub struct GitLabProjectInfo {
    pub default_branch: String,
}

#[derive(Deserialize)]
pub struct GitLabBranchInfo {
    pub commit: GitLabCommitInfo,
}

#[derive(Deserialize)]
pub struct GitLabCommitInfo {
    pub id: String,
}

#[derive(Serialize)]
pub struct GitLabCommitRequest {
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commit_id: Option<String>,
    pub commit_message: String,
    pub actions: Vec<GitLabCommitAction>,
}

#[derive(Serialize)]
pub struct GitLabCommitAction {
    pub action: String,
    pub file_path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct MergeRequestInfo {
    pub iid: u64,
    pub web_url: String,
}

#[derive(Serialize)]
pub struct MergeRequestPayload {
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<u64>,
    pub remove_source_branch: bool,
}

#[derive(Serialize)]
pub struct MergeRequestUpdatePayload {
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<u64>,
    pub remove_source_branch: bool,
}

pub struct MergeRequestResult {
    pub url: String,
    pub updated: bool,
}

#[derive(Deserialize)]
struct GitLabUser {
    id: u64,
    username: String,
}

impl<'a> GitLabClient<'a> {
    pub fn new(config: &'a GitLabConfig, token: &'a str, auth_header: &'a str) -> Self {
        Self {
            config,
            token,
            auth_header,
        }
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let base = self.config.trimmed_base();
        let full_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let url = format!("{base}{full_path}");
        ureq::request(method, &url)
            .set(self.auth_header, self.token)
            .set("Accept", "application/json")
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, context: &str) -> Result<T, AppError> {
        let response = self
            .request("GET", path)
            .call()
            .map_err(|err| map_ureq_error(err, context))?;
        parse_json(response, context)
    }

    fn get_optional_json<T: DeserializeOwned>(
        &self,
        path: &str,
        context: &str,
    ) -> Result<Option<T>, AppError> {
        match self.request("GET", path).call() {
            Ok(response) => parse_json(response, context).map(Some),
            Err(UreqError::Status(404, _)) => Ok(None),
            Err(err) => Err(map_ureq_error(err, context)),
        }
    }

    fn send_json<T: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: &T,
        context: &str,
    ) -> Result<Response, AppError> {
        let payload = serde_json::to_value(body).map_err(|err| {
            AppError::msg(format!(
                "{context}: failed to serialize request body: {err}"
            ))
        })?;
        self.request(method, path)
            .set("Content-Type", "application/json")
            .send_json(payload)
            .map_err(|err| map_ureq_error(err, context))
    }

    fn post_json<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        context: &str,
    ) -> Result<R, AppError> {
        let response = self.send_json("POST", path, body, context)?;
        parse_json(response, context)
    }

    fn put_json<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        context: &str,
    ) -> Result<R, AppError> {
        let response = self.send_json("PUT", path, body, context)?;
        parse_json(response, context)
    }

    pub fn project_info(&self) -> Result<GitLabProjectInfo, AppError> {
        let path = self.config.project_api("");
        self.get_json(&path, "fetch GitLab project info")
    }

    pub fn user_id_by_username(&self, username: &str) -> Result<Option<u64>, AppError> {
        let path = format!("/users?username={}", encode_component(username));
        let users: Vec<GitLabUser> = self.get_json(&path, "lookup GitLab assignee user")?;
        Ok(users
            .into_iter()
            .find(|user| user.username == username)
            .map(|user| user.id))
    }

    pub fn branch_info(&self, branch: &str) -> Result<Option<GitLabBranchInfo>, AppError> {
        let path = self.config.project_api(&format!(
            "/repository/branches/{}",
            encode_component(branch)
        ));
        let context = format!("fetch GitLab branch {branch}");
        self.get_optional_json(&path, &context)
    }

    pub fn create_commit(
        &self,
        payload: &GitLabCommitRequest,
    ) -> Result<GitLabCommitInfo, AppError> {
        let path = self.config.project_api("/repository/commits");
        self.post_json(&path, payload, "create commit in GitLab")
    }

    pub fn find_open_merge_request(
        &self,
        source_branch: &str,
        target_branch: &str,
    ) -> Result<Option<MergeRequestInfo>, AppError> {
        let path = format!(
            "{base}?state=opened&source_branch={source}&target_branch={target}",
            base = self.config.project_api("/merge_requests"),
            source = encode_component(source_branch),
            target = encode_component(target_branch)
        );
        let mrs: Vec<MergeRequestInfo> = self.get_json(&path, "query merge requests")?;
        Ok(mrs.into_iter().next())
    }

    pub fn create_merge_request(
        &self,
        payload: &MergeRequestPayload,
    ) -> Result<MergeRequestInfo, AppError> {
        let path = self.config.project_api("/merge_requests");
        self.post_json(&path, payload, "create merge request")
    }

    pub fn update_merge_request(
        &self,
        iid: u64,
        payload: &MergeRequestUpdatePayload,
    ) -> Result<MergeRequestInfo, AppError> {
        let path = self.config.project_api(&format!("/merge_requests/{iid}"));
        self.put_json(&path, payload, "update merge request")
    }

    pub fn file_contents(
        &self,
        file_path: &str,
        reference: &str,
    ) -> Result<Option<String>, AppError> {
        let path = self.config.project_api(&format!(
            "/repository/files/{}/raw?ref={}",
            encode_component(file_path),
            encode_component(reference)
        ));
        match self.request("GET", &path).call() {
            Ok(response) => response
                .into_string()
                .map(Some)
                .map_err(|err| AppError::msg(format!("read GitLab file response failed: {err}"))),
            Err(UreqError::Status(404, _)) => Ok(None),
            Err(err) => Err(map_ureq_error(err, "fetch raw file from GitLab")),
        }
    }
}

fn map_ureq_error(err: UreqError, context: &str) -> AppError {
    match err {
        UreqError::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            AppError::msg(format!("{context}: HTTP {code}: {body}"))
        }
        UreqError::Transport(transport) => {
            AppError::msg(format!("{context}: transport error: {}", transport))
        }
    }
}

fn parse_json<T: DeserializeOwned>(response: Response, context: &str) -> Result<T, AppError> {
    response
        .into_json::<T>()
        .map_err(|err| AppError::msg(format!("{context}: failed to decode response JSON: {err}")))
}

#[cfg(test)]
mod tests {
    use super::{encode_component, GitLabConfig};

    fn config() -> GitLabConfig {
        GitLabConfig {
            base_url: "https://git.example.com/api/v4/".to_string(),
            project: "ops/deployments".to_string(),
        }
    }

    #[test]
    fn component_encoding_escapes_non_alphanumerics() {
        assert_eq!(encode_component("abc123"), "abc123");
        assert_eq!(encode_component("ops/deployments"), "ops%2Fdeployments");
    }

    #[test]
    fn archive_url_encodes_project_and_trims_base_slash() {
        assert_eq!(
            config().archive_url(),
            "https://git.example.com/api/v4/projects/ops%2Fdeployments/repository/archive.tar.gz"
        );
    }

    #[test]
    fn archive_url_for_revision_appends_encoded_sha_query() {
        assert_eq!(
            config().archive_url_for_revision("abc/123"),
            "https://git.example.com/api/v4/projects/ops%2Fdeployments/repository/archive.tar.gz?sha=abc%2F123"
        );
    }

    #[test]
    fn trimmed_base_removes_trailing_slashes_only() {
        assert_eq!(config().trimmed_base(), "https://git.example.com/api/v4");
        let plain = GitLabConfig {
            base_url: "https://git.example.com/api/v4".to_string(),
            project: "p".to_string(),
        };
        assert_eq!(plain.trimmed_base(), "https://git.example.com/api/v4");
    }
}
