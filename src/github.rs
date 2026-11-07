//! A small blocking GitHub client: REST, raw file contents and GraphQL.

use serde_json::Value;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_ROOT: &str = "https://api.github.com";
const USER_AGENT: &str = "awesome-rune";

#[derive(Debug)]
pub struct Error {
    /// The HTTP status, when GitHub answered.
    pub status: Option<u16>,
    /// Seconds GitHub asked us to wait, from `Retry-After` or an exhausted rate limit.
    pub retry_after: Option<f64>,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl Error {
    fn other(message: impl fmt::Display) -> Error {
        Error { status: None, retry_after: None, message: message.to_string() }
    }
}

impl From<ureq::Error> for Error {
    fn from(error: ureq::Error) -> Error {
        match error {
            ureq::Error::Status(status, response) => {
                let retry_after = response.header("Retry-After").and_then(|v| v.parse().ok()).or_else(|| {
                    let exhausted = response.header("X-RateLimit-Remaining") == Some("0");
                    let reset: f64 = response.header("X-RateLimit-Reset")?.parse().ok()?;
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
                    exhausted.then(|| (reset - now + 1.0).max(1.0))
                });
                Error { status: Some(status), retry_after, message: response.status_text().to_string() }
            }
            ureq::Error::Transport(transport) => Error::other(transport),
        }
    }
}

pub struct GitHub {
    agent: ureq::Agent,
    pub token: Option<String>,
}

impl GitHub {
    pub fn new(token: Option<String>) -> GitHub {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).user_agent(USER_AGENT).build();
        GitHub { agent, token }
    }

    fn request(&self, method: &str, url: &str, accept: &str) -> ureq::Request {
        let request = self.agent.request(method, url).set("Accept", accept).set("X-GitHub-Api-Version", "2022-11-28");
        match &self.token {
            Some(token) => request.set("Authorization", &format!("Bearer {token}")),
            None => request,
        }
    }

    fn url(path: &str, params: &[(&str, &str)]) -> String {
        let mut url = format!("{API_ROOT}{path}");
        for (i, (key, value)) in params.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&format!("{}={}", encode(key), encode(value)));
        }
        url
    }

    pub fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let body = self.request("GET", &Self::url(path, params), "application/vnd.github+json").call()?.into_string().map_err(Error::other)?;
        serde_json::from_str(&body).map_err(Error::other)
    }

    /// A file's contents as text.
    pub fn get_raw(&self, path: &str, params: &[(&str, &str)]) -> Result<String, Error> {
        self.request("GET", &Self::url(path, params), "application/vnd.github.raw").call()?.into_string().map_err(Error::other)
    }

    pub fn graphql(&self, query: &str) -> Result<Value, Error> {
        if self.token.is_none() {
            return Err(Error::other("GraphQL needs a GITHUB_TOKEN"));
        }
        let body = serde_json::json!({ "query": query }).to_string();
        let response = self.request("POST", &format!("{API_ROOT}/graphql"), "application/json").set("Content-Type", "application/json").send_string(&body)?;
        let mut value: Value = serde_json::from_str(&response.into_string().map_err(Error::other)?).map_err(Error::other)?;
        if let Some(error) = value["errors"].get(0) {
            return Err(Error::other(error["message"].as_str().unwrap_or("GraphQL error")));
        }
        Ok(value["data"].take())
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

pub fn target(url: &str) -> Option<Target> {
    let rest = url.strip_prefix("https://github.com/").or_else(|| url.strip_prefix("https://www.github.com/"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<String> = rest.split('/').filter(|p| !p.is_empty()).map(decode).collect();
    if parts.len() < 2 {
        return None;
    }
    let (owner, name) = (&parts[0], parts[1].trim_end_matches(".git"));
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let tree = parts.len() >= 4 && parts[2] == "tree";
    Some(Target {
        repository: format!("{owner}/{name}"),
        branch: tree.then(|| parts[3].clone()),
        path: (tree && parts.len() >= 5).then(|| parts[4..].join("/")),
    })
}

/// Percent-encodes everything but unreserved characters.
pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
