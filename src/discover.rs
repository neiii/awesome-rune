//! Reports GitHub repositories that depend on Rune but are neither listed nor ignored.

use crate::github::{self, GitHub, Repository};
use crate::{read_json, token};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const QUERY: &str = "\"anomalyco/rune\" filename:Cargo.toml";
const UPSTREAM: &str = "anomalyco/rune";
const PAGE_SIZE: usize = 100;
/// The search API returns at most 1,000 results.
const MAX_PAGES: usize = 10;
/// Code search allows 10 requests a minute.
const PAGE_DELAY: Duration = Duration::from_secs(7);
/// The workflow token is sometimes throttled even on its first request, so wait out rate limits
/// instead of failing the run.
const SEARCH_ATTEMPTS: u32 = 5;
const MAX_WAIT_SECONDS: f64 = 120.0;

#[derive(Deserialize)]
struct SearchPage {
    items: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    path: String,
    html_url: String,
    repository: HitRepository,
}

#[derive(Deserialize)]
struct HitRepository {
    full_name: String,
}

/// How long GitHub asked us to wait after a 403 or 429, if it is a rate limit.
fn rate_limit_wait(error: &anyhow::Error, attempt: u32) -> Option<f64> {
    let Some(ureq::Error::Status(403 | 429, response)) = error.downcast_ref::<ureq::Error>() else { return None };
    let header = |name: &str| response.header(name).and_then(|v| v.parse::<f64>().ok());
    let reset = || -> Option<f64> {
        if response.header("X-RateLimit-Remaining") != Some("0") {
            return None;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
        Some((header("X-RateLimit-Reset")? - now + 1.0).max(1.0))
    };
    Some(header("Retry-After").or_else(reset).unwrap_or(15.0 * 2f64.powi(attempt as i32)).min(MAX_WAIT_SECONDS))
}

fn search_page(github: &GitHub, page: usize) -> Result<SearchPage> {
    let url = GitHub::url(&["search", "code"], &[("q", QUERY), ("per_page", &PAGE_SIZE.to_string()), ("page", &page.to_string())]);
    let mut attempt = 0;
    loop {
        let error = match github.get(&url) {
            Ok(page) => return Ok(page),
            Err(error) => error,
        };
        match rate_limit_wait(&error, attempt) {
            Some(delay) if attempt + 1 < SEARCH_ATTEMPTS => {
                eprintln!("Rate limited on page {page} ({error}); retrying in {delay:.0}s.");
                sleep(Duration::from_secs_f64(delay));
                attempt += 1;
            }
            _ => return Err(error),
        }
    }
}

/// Each matching repository and the manifests that matched.
fn search(github: &GitHub) -> Result<BTreeMap<String, Vec<Hit>>> {
    let mut hits: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
    for page in 1..=MAX_PAGES {
        if page > 1 {
            sleep(PAGE_DELAY);
        }
        let items = search_page(github, page)?.items;
        let last = items.len() < PAGE_SIZE;
        for hit in items {
            hits.entry(hit.repository.full_name.clone()).or_default().push(hit);
        }
        if last {
            break;
        }
    }
    Ok(hits)
}

fn known_repositories() -> Result<HashSet<String>> {
    #[derive(Deserialize)]
    struct Ignored {
        repositories: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        repository: String,
    }
    #[derive(Deserialize)]
    struct Projects {
        projects: Vec<Project>,
    }
    #[derive(Deserialize)]
    struct Project {
        url: String,
    }

    let ignored = read_json::<Ignored>("ignored.json")?.repositories.into_iter().map(|e| e.repository);
    let listed = read_json::<Projects>("projects.json")?.projects.into_iter().filter_map(|p| github::target(&p.url)).map(|t| t.repository);
    Ok(std::iter::once(UPSTREAM.to_string()).chain(ignored).chain(listed).map(|r| r.to_lowercase()).collect())
}

fn escape_cell(text: &str) -> String {
    text.replace('\\', "\\\\").replace('|', "\\|").replace('\n', " ")
}

fn render(found: &[(Repository, Vec<Hit>)]) -> String {
    let mut report = String::from(
        "Repositories whose `Cargo.toml` mentions `anomalyco/rune` but that are in neither `projects.json` nor `ignored.json`. Updated weekly by `.github/workflows/discover.yml`.\n\n\
         For each one, add it to `projects.json` or record why it is left out in `ignored.json`; either way it stops appearing here.\n\n\
         | Repository | Stars | Last push | Description | Matched |\n\
         | --- | ---: | --- | --- | --- |\n",
    );
    for (repo, hits) in found {
        let matched: Vec<String> = hits.iter().map(|h| format!("[`{}`]({})", escape_cell(&h.path), h.html_url)).collect();
        let pushed = repo.pushed_at.as_deref().map_or("", |p| &p[..p.len().min(10)]);
        let description = escape_cell(repo.description.as_deref().unwrap_or(""));
        report += &format!("| [{}]({}) | {} | {pushed} | {description} | {} |\n", repo.full_name, repo.html_url, repo.stargazers_count, matched.join(", "));
    }
    report
}

pub fn run(output: Option<PathBuf>) -> Result<ExitCode> {
    let Some(token) = token() else {
        eprintln!("GITHUB_TOKEN is required: the code search API rejects anonymous requests.");
        return Ok(ExitCode::FAILURE);
    };
    let github = GitHub::new(Some(token));
    let known = known_repositories()?;
    let mut found = vec![];
    for (name, hits) in search(&github).context("code search")? {
        if known.contains(&name.to_lowercase()) {
            continue;
        }
        let repo = match github.repository(&name) {
            Ok(repo) => repo,
            Err(error) => {
                eprintln!("warning: skipping {name}: {error}");
                continue;
            }
        };
        // Renamed repositories resolve to their canonical name, which may be listed.
        if !known.contains(&repo.full_name.to_lowercase()) && !repo.fork && !repo.archived {
            found.push((repo, hits));
        }
    }
    found.sort_by_key(|(repo, _)| (std::cmp::Reverse(repo.stargazers_count), repo.full_name.to_lowercase()));

    let report = render(&found);
    match &output {
        Some(path) => std::fs::write(path, report).with_context(|| format!("writing {}", path.display()))?,
        None => print!("{report}"),
    }
    if let Ok(path) = std::env::var("GITHUB_OUTPUT") {
        let mut file = std::fs::OpenOptions::new().append(true).create(true).open(path).context("opening GITHUB_OUTPUT")?;
        writeln!(file, "count={}", found.len())?;
    }
    eprintln!("Found {} new candidate(s).", found.len());
    Ok(ExitCode::SUCCESS)
}
