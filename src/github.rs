//! A small blocking GitHub client: REST, raw file contents and GraphQL.

use anyhow::{anyhow, Result};
use percent_encoding::percent_decode_str;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

const API: &str = "https://api.github.com";

/// The subset of `GET /repos/{owner}/{repo}` the tool reads.
#[derive(Deserialize)]
pub struct Repository {
    pub full_name: String,
    pub html_url: String,
    pub description: Option<String>,
    pub stargazers_count: u64,
    pub archived: bool,
    pub fork: bool,
    pub default_branch: String,
    pub pushed_at: Option<String>,
}

pub struct GitHub {
    agent: ureq::Agent,
    token: Option<String>,
}

impl GitHub {
    pub fn new(token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).user_agent("awesome-rune").build();
        Self { agent, token }
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// An API URL from path segments (each percent-encoded) and query pairs.
    pub fn url(segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(API).expect("valid API root");
        url.path_segments_mut().expect("API root is a base URL").extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    fn request(&self, method: &str, url: &Url, accept: &str) -> ureq::Request {
        let request = self.agent.request_url(method, url).set("Accept", accept).set("X-GitHub-Api-Version", "2022-11-28");
        match &self.token {
            Some(token) => request.set("Authorization", &format!("Bearer {token}")),
            None => request,
        }
    }

    pub fn get<T: DeserializeOwned>(&self, url: &Url) -> Result<T> {
        Ok(self.request("GET", url, "application/vnd.github+json").call()?.into_json()?)
    }

    pub fn repository(&self, name: &str) -> Result<Repository> {
        let (owner, repo) = name.split_once('/').ok_or_else(|| anyhow!("{name} is not owner/name"))?;
        self.get(&Self::url(&["repos", owner, repo], &[]))
    }

    pub fn graphql<T: DeserializeOwned>(&self, query: &str) -> Result<T> {
        #[derive(Deserialize)]
        struct Response<T> {
            data: Option<T>,
            #[serde(default)]
            errors: Vec<Message>,
        }
        #[derive(Deserialize)]
        struct Message {
            message: String,
        }

        anyhow::ensure!(self.has_token(), "GraphQL needs a GITHUB_TOKEN");
        let url = Url::parse(&format!("{API}/graphql"))?;
        let response: Response<T> = self.request("POST", &url, "application/json").send_json(serde_json::json!({ "query": query }))?.into_json()?;
        if let Some(error) = response.errors.into_iter().next() {
            return Err(anyhow!(error.message));
        }
        response.data.ok_or_else(|| anyhow!("GraphQL returned no data"))
    }
}

/// The HTTP status of a failed request, if GitHub answered.
pub fn status(error: &anyhow::Error) -> Option<u16> {
    match error.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::Status(code, _)) => Some(*code),
        _ => None,
    }
}

/// Turns a 404 into `None`.
pub fn optional<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if status(&error) == Some(404) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Where a project URL points on GitHub.
pub struct Target {
    /// `owner/name`.
    pub repository: String,
    /// The branch of a `/tree/<branch>/...` URL.
    pub branch: Option<String>,
    /// The directory inside the repository, for apps that live in a larger repository.
    pub path: Option<String>,
}

impl Target {
    pub fn owner(&self) -> &str {
        self.repository.split('/').next().unwrap_or_default()
    }

    /// `owner/name/path`, for messages.
    pub fn label(&self) -> String {
        match &self.path {
            Some(path) => format!("{}/{path}", self.repository),
            None => self.repository.clone(),
        }
    }
}

pub fn target(url: &str) -> Option<Target> {
    let url = Url::parse(url).ok()?;
    if !matches!(url.host_str()?, "github.com" | "www.github.com") {
        return None;
    }
    let parts: Vec<String> = url.path_segments()?.filter(|s| !s.is_empty()).map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let [owner, name, rest @ ..] = parts.as_slice() else { return None };
    let name = name.trim_end_matches(".git");
    if name.is_empty() {
        return None;
    }
    let (branch, path) = match rest {
        [tree, branch, path @ ..] if tree == "tree" => (Some(branch.clone()), (!path.is_empty()).then(|| path.join("/"))),
        _ => (None, None),
    };
    Some(Target { repository: format!("{owner}/{name}"), branch, path })
}
