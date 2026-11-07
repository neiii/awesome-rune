//! Reports GitHub repositories that depend on Rune but are neither listed nor ignored.

use crate::github::{self, GitHub};
use crate::{root, token};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

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

struct Manifest {
    path: String,
    url: String,
}

fn search_page(github: &GitHub, page: usize) -> Result<Value, github::Error> {
    let (per_page, page_s) = (PAGE_SIZE.to_string(), page.to_string());
    let params = [("q", QUERY), ("per_page", per_page.as_str()), ("page", page_s.as_str())];
    let mut attempt = 0;
    loop {
        match github.get("/search/code", &params) {
            // GitHub signals rate limits with 429, or 403 for secondary limits.
            Err(error) if matches!(error.status, Some(403 | 429)) && attempt + 1 < SEARCH_ATTEMPTS => {
                let delay = error.retry_after.unwrap_or(15.0 * 2f64.powi(attempt as i32)).min(MAX_WAIT_SECONDS);
                eprintln!("Rate limited on page {page} ({error}); retrying in {delay:.0}s.");
                sleep(Duration::from_secs_f64(delay));
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Each matching repository and the manifests that matched.
fn search(github: &GitHub) -> Result<BTreeMap<String, Vec<Manifest>>, github::Error> {
    let mut hits: BTreeMap<String, Vec<Manifest>> = BTreeMap::new();
    for page in 1..=MAX_PAGES {
        if page > 1 {
            sleep(PAGE_DELAY);
        }
        let data = search_page(github, page)?;
        let items = data["items"].as_array().cloned().unwrap_or_default();
        for item in &items {
            let repository = item["repository"]["full_name"].as_str().unwrap_or_default().to_string();
            let manifest = Manifest { path: item["path"].as_str().unwrap_or_default().into(), url: item["html_url"].as_str().unwrap_or_default().into() };
            hits.entry(repository).or_default().push(manifest);
        }
        if items.len() < PAGE_SIZE {
            break;
        }
    }
    Ok(hits)
}

fn known_repositories() -> Result<HashSet<String>, String> {
    let read = |name: &str| -> Result<Value, String> {
        serde_json::from_str(&std::fs::read_to_string(root().join(name)).map_err(|e| format!("{name}: {e}"))?).map_err(|e| format!("{name}: {e}"))
    };
    let mut known: HashSet<String> = HashSet::from([UPSTREAM.to_lowercase()]);
    for entry in read("ignored.json")?["repositories"].as_array().into_iter().flatten() {
        known.insert(entry["repository"].as_str().unwrap_or_default().to_lowercase());
    }
    for project in read("projects.json")?["projects"].as_array().into_iter().flatten() {
        if let Some(target) = github::target(project["url"].as_str().unwrap_or_default()) {
            known.insert(target.repository.to_lowercase());
        }
    }
    Ok(known)
}

fn escape_cell(text: &str) -> String {
    text.replace('\\', "\\\\").replace('|', "\\|").replace('\n', " ")
}

fn render(found: &[(Value, Vec<Manifest>)]) -> String {
    let mut lines = vec![
        "Repositories whose `Cargo.toml` mentions `anomalyco/rune` but that are in neither `projects.json` nor `ignored.json`. Updated weekly by `.github/workflows/discover.yml`.".to_string(),
        String::new(),
        "For each one, add it to `projects.json` or record why it is left out in `ignored.json`; either way it stops appearing here.".into(),
        String::new(),
        "| Repository | Stars | Last push | Description | Matched |".into(),
        "| --- | ---: | --- | --- | --- |".into(),
    ];
    for (metadata, manifests) in found {
        let matched: Vec<String> = manifests.iter().map(|m| format!("[`{}`]({})", escape_cell(&m.path), m.url)).collect();
        let pushed = metadata["pushed_at"].as_str().unwrap_or("");
        lines.push(format!(
            "| [{name}]({url}) | {stars} | {pushed} | {description} | {matched} |",
            name = metadata["full_name"].as_str().unwrap_or_default(),
            url = metadata["html_url"].as_str().unwrap_or_default(),
            stars = metadata["stargazers_count"].as_u64().unwrap_or(0),
            pushed = &pushed[..pushed.len().min(10)],
            description = escape_cell(metadata["description"].as_str().unwrap_or("")),
            matched = matched.join(", "),
        ));
    }
    lines.join("\n") + "\n"
}

pub fn run(output: Option<PathBuf>) -> Result<ExitCode, String> {
    let Some(token) = token() else {
        eprintln!("GITHUB_TOKEN is required: the code search API rejects anonymous requests.");
        return Ok(ExitCode::FAILURE);
    };
    let github = GitHub::new(Some(token));
    let known = known_repositories()?;
    let mut found = vec![];
    for (repository, manifests) in search(&github).map_err(|e| format!("code search: {e}"))? {
        if known.contains(&repository.to_lowercase()) {
            continue;
        }
        let metadata = match github.get(&format!("/repos/{repository}"), &[]) {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!("warning: skipping {repository}: {error}");
                continue;
            }
        };
        // Renamed repositories resolve to their canonical name, which may be listed.
        let canonical = metadata["full_name"].as_str().unwrap_or_default().to_lowercase();
        if known.contains(&canonical) || metadata["fork"].as_bool() == Some(true) || metadata["archived"].as_bool() == Some(true) {
            continue;
        }
        found.push((metadata, manifests));
    }
    found.sort_by_key(|(m, _)| (std::cmp::Reverse(m["stargazers_count"].as_u64().unwrap_or(0)), m["full_name"].as_str().unwrap_or_default().to_lowercase()));

    let report = render(&found);
    match output {
        Some(path) => std::fs::write(&path, report).map_err(|e| format!("{}: {e}", path.display()))?,
        None => print!("{report}"),
    }
    if let Ok(path) = std::env::var("GITHUB_OUTPUT") {
        let mut file = std::fs::OpenOptions::new().append(true).create(true).open(path).map_err(|e| e.to_string())?;
        writeln!(file, "count={}", found.len()).map_err(|e| e.to_string())?;
    }
    eprintln!("Found {} new candidate(s).", found.len());
    Ok(ExitCode::SUCCESS)
}
