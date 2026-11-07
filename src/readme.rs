//! Generates README.md and the activity charts from projects.json and current GitHub metadata.
//!
//! Each project gets a card: its icon, a plastic star badge for its tier, a bar chart of commits
//! per day over the last 28 days, and a line with active days, its latest release and how far its
//! pinned Rune revision trails `dev`. The badge art is rendered once by tools/badges; this only
//! assembles charts from those sprites, so it needs no GPU.
//!
//! Charts are written to build/charts and published by the workflow to the `assets` branch.
//! Without a GITHUB_TOKEN (GraphQL needs one) cards are generated without charts.

use crate::github::{self, GitHub, Target};
use crate::{root, token};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::process::ExitCode;
use std::sync::Mutex;

const PLACEHOLDER: &str = "<!-- PROJECTS -->";
const RUNE_REPO: &str = "anomalyco/rune";
const RUNE_URL: &str = "https://github.com/anomalyco/rune";
/// Published by the update-readme workflow; the query string busts GitHub's image cache.
const ASSETS_URL: &str = "https://raw.githubusercontent.com/neiii/awesome-rune/assets";

const DAYS: usize = 28;
const FEATURED: usize = 3;
/// Star tiers, lowest first: (minimum stars, badge).
const STAR_TIERS: [(u64, &str); 4] = [(0, "gunmetal"), (10, "rose"), (100, "silver"), (1000, "gold")];
/// Bars: one per day, 4 pt wide on a 6 pt pitch, 6..32 pt tall on a log scale where 60 commits
/// reach the top; a quiet day is the 4 pt stub. Heights round to the sprites' half-point steps.
const BAR_PITCH_PT: f64 = 6.0;
const BAR_MIN_PT: f64 = 6.0;
const BAR_MAX_PT: f64 = 32.0;
const BAR_FULL_COMMITS: f64 = 60.0;
/// Projects fetched at once.
const PARALLEL: usize = 8;

#[derive(Deserialize)]
struct Projects {
    projects: Vec<Project>,
}

#[derive(Deserialize)]
struct Project {
    name: String,
    category: String,
    subcategory: Option<String>,
    url: String,
    description: String,
    image: Option<String>,
}

impl Project {
    fn official(&self) -> bool {
        self.url.starts_with(&format!("{RUNE_URL}/"))
    }
}

struct Release {
    tag: String,
    date: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Meta {
    stars: Option<u64>,
    archived: bool,
    available: bool,
    days: Option<Vec<u64>>,
    release: Option<Release>,
    behind: Option<u64>,
}

fn error(message: impl Into<String>) -> github::Error {
    github::Error { status: None, retry_after: None, message: message.into() }
}

// ------------------------------------------------------------------ metadata

/// Exact commits per UTC day for the last DAYS days, oldest first, in one GraphQL query.
fn daily_commits(github: &GitHub, target: &Target, branch: &str) -> Result<Vec<u64>, github::Error> {
    let (owner, name) = target.repository.split_once('/').unwrap_or_default();
    let midnight = Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap_or_default().and_utc();
    let end = midnight + Duration::days(1);
    let path = target.path.as_ref().map(|p| format!(", path: {}", Value::from(p.as_str()))).unwrap_or_default();
    let iso = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let fields: Vec<String> = (0..DAYS)
        .map(|day| {
            let until = end - Duration::days((DAYS - 1 - day) as i64);
            let since = until - Duration::days(1);
            format!(r#"d{day}: history(since: "{}", until: "{}"{path}) {{ totalCount }}"#, iso(since), iso(until))
        })
        .collect();
    let query = format!(
        "{{ repository(owner: {}, name: {}) {{ object(expression: {}) {{ ... on Commit {{ {} }} }} }} }}",
        Value::from(owner),
        Value::from(name),
        Value::from(branch),
        fields.join(" ")
    );
    let data = github.graphql(&query)?;
    let commit = &data["repository"]["object"];
    (0..DAYS)
        .map(|day| commit[format!("d{day}")]["totalCount"].as_u64())
        .collect::<Option<Vec<u64>>>()
        .ok_or_else(|| error(format!("no history at {branch}")))
}

fn latest_release(github: &GitHub, repository: &str) -> Result<Option<Release>, github::Error> {
    match github.get(&format!("/repos/{repository}/releases/latest"), &[]) {
        Ok(release) => Ok(Some(Release {
            tag: release["tag_name"].as_str().unwrap_or_default().to_string(),
            date: release["published_at"].as_str().and_then(|d| DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&Utc)),
        })),
        Err(e) if e.status == Some(404) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The Rune revision the project's Cargo.lock pins, from its own directory or the root.
fn rune_pin(github: &GitHub, target: &Target, branch: &str, pin: &Regex) -> Result<Option<String>, github::Error> {
    let locks = target.path.iter().map(|p| format!("{p}/Cargo.lock")).chain(["Cargo.lock".to_string()]);
    for lock in locks {
        let path = format!("/repos/{}/contents/{}", target.repository, github::encode(&lock).replace("%2F", "/"));
        match github.get_raw(&path, &[("ref", branch)]) {
            Ok(text) => {
                if let Some(found) = pin.captures(&text) {
                    return Ok(Some(found[1].to_string()));
                }
            }
            Err(e) if e.status == Some(404) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Everything one card needs from GitHub, for one project.
fn project_meta(github: &GitHub, project: &Project, target: &Target, repo: &Value, pin: &Regex, behind: &Mutex<HashMap<String, Option<u64>>>, errors: &Mutex<BTreeSet<String>>) -> Meta {
    let mut meta = Meta {
        stars: repo["stargazers_count"].as_u64(),
        archived: repo["archived"].as_bool().unwrap_or(false),
        available: true,
        ..Meta::default()
    };
    let branch = target.branch.clone().or_else(|| repo["default_branch"].as_str().map(str::to_string)).unwrap_or_else(|| "main".into());
    let label = match &target.path {
        Some(path) => format!("{}/{path}", target.repository),
        None => target.repository.clone(),
    };
    let warn = |what: &str, e: github::Error| {
        errors.lock().unwrap().insert(format!("{label} {what}: {e}"));
    };

    match latest_release(github, &target.repository) {
        Ok(release) => meta.release = release,
        Err(e) => warn("release", e),
    }
    if github.token.is_some() {
        match daily_commits(github, target, &branch) {
            Ok(days) => meta.days = Some(days),
            Err(e) => warn("activity", e),
        }
    }
    if project.official() {
        return meta;
    }
    let sha = match rune_pin(github, target, &branch, pin) {
        Ok(Some(sha)) => sha,
        Ok(None) => return meta,
        Err(e) => {
            warn("Rune pin", e);
            return meta;
        }
    };
    let cached = behind.lock().unwrap().get(&sha).copied();
    let lag = match cached {
        Some(lag) => Ok(lag),
        None => github.get(&format!("/repos/{RUNE_REPO}/compare/{sha}...dev"), &[]).map(|c| c["ahead_by"].as_u64()),
    };
    match lag {
        Ok(lag) => {
            behind.lock().unwrap().insert(sha, lag);
            meta.behind = lag;
        }
        Err(e) => warn("Rune pin", e),
    }
    meta
}

fn collect_metadata(github: &GitHub, projects: &[Project]) -> Vec<Meta> {
    let errors = Mutex::new(BTreeSet::new());
    let targets: Vec<Option<Target>> = projects.iter().map(|p| github::target(&p.url)).collect();

    let mut repositories: Vec<&str> = targets.iter().flatten().map(|t| t.repository.as_str()).collect();
    repositories.sort_unstable();
    repositories.dedup();
    let repos: Mutex<HashMap<&str, Value>> = Mutex::new(HashMap::new());
    for chunk in repositories.chunks(PARALLEL) {
        std::thread::scope(|scope| {
            for &repository in chunk {
                let (repos, errors) = (&repos, &errors);
                scope.spawn(move || match github.get(&format!("/repos/{repository}"), &[]) {
                    Ok(value) => {
                        repos.lock().unwrap().insert(repository, value);
                    }
                    Err(e) => {
                        errors.lock().unwrap().insert(format!("{repository}: {e}"));
                    }
                });
            }
        });
    }
    let repos = repos.into_inner().unwrap();

    let pin = Regex::new(&format!(r#"git\+https://github\.com/{RUNE_REPO}[^#"]*#([0-9a-f]{{7,40}})"#)).expect("valid pattern");
    let behind = Mutex::new(HashMap::new());
    let mut results: Vec<Meta> = Vec::with_capacity(projects.len());
    let pairs: Vec<(&Project, &Option<Target>)> = projects.iter().zip(&targets).collect();
    for chunk in pairs.chunks(PARALLEL) {
        let metas: Vec<Meta> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&(project, target)| {
                    let (repos, pin, behind, errors) = (&repos, &pin, &behind, &errors);
                    scope.spawn(move || match target.as_ref().and_then(|t| Some((t, repos.get(t.repository.as_str())?))) {
                        Some((target, repo)) => project_meta(github, project, target, repo, pin, behind, errors),
                        None => Meta::default(),
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("fetch thread panicked")).collect()
        });
        results.extend(metas);
    }

    for e in errors.into_inner().unwrap() {
        eprintln!("warning: GitHub metadata unavailable for {e}");
    }
    results
}

// ------------------------------------------------------------------ charts

struct Sprites {
    sheet: Vec<u8>,
    width: usize,
    /// Pixel rects (x, y, w, h), keyed by height in half points or "stub".
    index: HashMap<String, [usize; 4]>,
    scale: f64,
}

fn load_sprites() -> Result<Sprites, String> {
    let dir = root().join("assets/bars");
    let json = std::fs::read_to_string(dir.join("bars.json")).map_err(|e| format!("bars.json: {e}"))?;
    let json: Value = serde_json::from_str(&json).map_err(|e| format!("bars.json: {e}"))?;
    let file = std::fs::File::open(dir.join("bars.png")).map_err(|e| format!("bars.png: {e}"))?;
    let mut reader = png::Decoder::new(file).read_info().map_err(|e| format!("bars.png: {e}"))?;
    let mut sheet = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut sheet).map_err(|e| format!("bars.png: {e}"))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("bars.png must be 8-bit RGBA".into());
    }
    let mut index = HashMap::new();
    for (key, rect) in json["sprites"].as_object().ok_or("bars.json: no sprites")? {
        let r: Vec<usize> = rect.as_array().into_iter().flatten().filter_map(Value::as_u64).map(|v| v as usize).collect();
        let rect: [usize; 4] = r.try_into().map_err(|_| format!("bars.json: bad rect for {key}"))?;
        index.insert(key.clone(), rect);
    }
    let scale = json["scale"].as_f64().ok_or("bars.json: no scale")?;
    Ok(Sprites { sheet, width: info.width as usize, index, scale })
}

fn bar_key(commits: u64) -> String {
    if commits == 0 {
        return "stub".into();
    }
    let level = ((commits as f64).ln_1p() / BAR_FULL_COMMITS.ln_1p()).min(1.0);
    format!("{}", ((BAR_MIN_PT + (BAR_MAX_PT - BAR_MIN_PT) * level) * 2.0).round() as u64)
}

/// One PNG: a bar per day, bottoms on the baseline. Bars never overlap, so each sprite is copied.
fn render_chart(days: &[u64], sprites: &Sprites) -> Result<Vec<u8>, String> {
    let stub_w = sprites.index.get("stub").ok_or("bars.json: no stub")?[2];
    let width = (BAR_PITCH_PT * (DAYS - 1) as f64 * sprites.scale).round() as usize + stub_w;
    let height = (BAR_MAX_PT * sprites.scale).round() as usize;
    let mut chart = vec![0u8; width * height * 4];
    for (day, &commits) in days.iter().enumerate() {
        let key = bar_key(commits);
        let [x, y, w, h] = *sprites.index.get(&key).ok_or_else(|| format!("bars.json: no sprite {key}"))?;
        let left = (day as f64 * BAR_PITCH_PT * sprites.scale).round() as usize;
        for row in 0..h {
            let src = ((y + row) * sprites.width + x) * 4;
            let dst = ((height - h + row) * width + left) * 4;
            chart[dst..dst + w * 4].copy_from_slice(&sprites.sheet[src..src + w * 4]);
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Best);
    encoder.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&chart).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(png)
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Writes one chart per project with activity; returns each chart's URL with a content hash.
fn write_charts(projects: &[Project], metas: &[Meta]) -> Result<Vec<Option<String>>, String> {
    let sprites = load_sprites()?;
    let dir = root().join("build/charts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for stale in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        if stale.path().extension().is_some_and(|x| x == "png") {
            std::fs::remove_file(stale.path()).map_err(|e| e.to_string())?;
        }
    }
    projects
        .iter()
        .zip(metas)
        .map(|(project, meta)| {
            let Some(days) = &meta.days else { return Ok(None) };
            let png = render_chart(days, &sprites)?;
            let name = format!("{}.png", slug(&project.name));
            std::fs::write(dir.join(&name), &png).map_err(|e| e.to_string())?;
            let hash: String = Sha256::digest(&png).iter().take(6).map(|b| format!("{b:02x}")).collect();
            Ok(Some(format!("{ASSETS_URL}/charts/{name}?v={hash}")))
        })
        .collect()
}

// ------------------------------------------------------------------ cards

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

fn compact_stars(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    let (number, suffix) = if value < 1_000_000 { (value as f64 / 1e3, "k") } else { (value as f64 / 1e6, "m") };
    let text = format!("{number:.*}", if number < 100.0 { 1 } else { 0 });
    let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.') } else { &text };
    format!("{text}{suffix}")
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn icon_url(project: &Project) -> String {
    if let Some(image) = &project.image {
        return image.clone();
    }
    match github::target(&project.url) {
        Some(target) if !project.official() => {
            format!("https://github.com/{}.png?size=128", target.repository.split('/').next().unwrap_or_default())
        }
        _ => "assets/rune-mark.svg".into(),
    }
}

fn star_tier(stars: u64) -> &'static str {
    STAR_TIERS.iter().rev().find(|(minimum, _)| stars >= *minimum).map_or("gunmetal", |(_, name)| name)
}

fn days_ago(date: DateTime<Utc>) -> String {
    match (Utc::now() - date).num_days().max(0) {
        0 => "today".into(),
        days => format!("{days}d ago"),
    }
}

fn card(project: &Project, meta: &Meta, chart: Option<&str>, icon_size: u32) -> String {
    let url = escape(&project.url);
    let mut lines = vec![
        format!(r#"<a href="{url}"><img src="{}" width="{icon_size}" height="{icon_size}" alt=""></a>"#, escape(&icon_url(project))),
        format!(r#"<a href="{url}"><b>{}</b></a>"#, escape(&project.name)),
    ];
    // Official crates share the Rune repository's stars, so only their own activity is shown.
    if let (Some(stars), false) = (meta.stars, project.official()) {
        let count = compact_stars(stars);
        lines.push(format!(r#"<img src="assets/stars/star-{}.png" height="32" align="middle" alt="{} stars"> <b>{count}</b>"#, star_tier(stars), escape(&count)));
    }
    if let Some(chart) = chart {
        lines.push(format!(r#"<img src="{}" height="32" alt="Commits per day over the last {DAYS} days">"#, escape(chart)));
    }

    let mut facts = vec![];
    if let Some(days) = &meta.days {
        facts.push(format!("<b>{}</b>/{DAYS} days", days.iter().filter(|&&c| c > 0).count()));
    }
    if let Some(release) = &meta.release {
        let when = release.date.map(|d| format!(" {}", days_ago(d))).unwrap_or_default();
        facts.push(format!("<code>{}</code>{when}", escape(&release.tag)));
    }
    if let Some(behind) = meta.behind {
        facts.push(format!("Rune pin {behind} behind"));
    }
    if meta.archived {
        facts.push("archived".into());
    } else if !meta.available {
        facts.push("unavailable".into());
    }
    if !facts.is_empty() {
        lines.push(format!("<sub>{}</sub>", facts.join(" · ")));
    }
    lines.push(format!("<sub>{}</sub>", escape(&project.description)));
    format!("<td align=\"center\" valign=\"top\" width=\"33%\">\n{}\n</td>", lines.join("<br>\n"))
}

fn grid(indices: &[usize], projects: &[Project], metas: &[Meta], charts: &[Option<String>], icon_size: u32) -> String {
    let rows: Vec<String> = indices
        .chunks(3)
        .map(|row| {
            let cells: Vec<String> = row.iter().map(|&i| card(&projects[i], &metas[i], charts[i].as_deref(), icon_size)).collect();
            format!("<tr>\n{}\n</tr>", cells.join("\n"))
        })
        .collect();
    format!("<table>\n{}\n</table>", rows.join("\n"))
}

fn anchor(heading: &str) -> String {
    heading.to_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == ' ' || *c == '-').collect::<String>().replace(' ', "-")
}

/// Project indices per subcategory (None for categories without them).
type Subcategories<'a> = Vec<(Option<&'a str>, Vec<usize>)>;

fn render_catalog(projects: &[Project], metas: &[Meta], charts: &[Option<String>]) -> String {
    // Categories and subcategories in the order projects.json lists them.
    let mut catalog: Vec<(&str, Subcategories)> = vec![];
    for (i, project) in projects.iter().enumerate() {
        let position = catalog.iter().position(|(c, _)| *c == project.category).unwrap_or_else(|| {
            catalog.push((&project.category, vec![]));
            catalog.len() - 1
        });
        let subcategories = &mut catalog[position].1;
        let sub = project.subcategory.as_deref();
        match subcategories.iter_mut().find(|(s, _)| *s == sub) {
            Some((_, entries)) => entries.push(i),
            None => subcategories.push((sub, vec![i])),
        }
    }

    let mut apps: Vec<usize> = (0..projects.len()).filter(|&i| projects[i].category == "Apps" && !projects[i].official()).collect();
    apps.sort_by_key(|&i| std::cmp::Reverse(metas[i].stars.unwrap_or(0)));
    apps.truncate(FEATURED);
    let mut sections = vec!["## Featured".to_string(), grid(&apps, projects, metas, charts, 96)];
    for (category, subcategories) in &catalog {
        sections.push(format!("## {category}"));
        if let Some((_, entries)) = subcategories.iter().find(|(s, _)| s.is_none()) {
            sections.push(grid(entries, projects, metas, charts, 56));
            continue;
        }
        sections.push(subcategories.iter().filter_map(|(s, _)| *s).map(|s| format!("[{s}](#{})", anchor(s))).collect::<Vec<_>>().join(" · "));
        for (subcategory, entries) in subcategories {
            sections.push(format!("### {}", subcategory.unwrap_or_default()));
            sections.push(grid(entries, projects, metas, charts, 56));
        }
    }

    let tiers: Vec<String> = STAR_TIERS
        .iter()
        .map(|&(minimum, name)| if minimum == 0 { format!("{name} under {}", STAR_TIERS[1].0) } else { format!("{name} {}+", thousands(minimum)) })
        .collect();
    sections.push(format!(
        "<sub>Stars: {}. Bars: commits per day over the last {DAYS} days, oldest on the left. Rune pin: how many commits the project's locked Rune revision trails <code>dev</code>.</sub>",
        tiers.join(", ")
    ));
    sections.join("\n\n")
}

pub fn run(check: bool) -> Result<ExitCode, String> {
    let token = token();
    if token.is_none() {
        eprintln!("warning: no GITHUB_TOKEN; generating without activity charts");
    }
    let github = GitHub::new(token);
    let text = std::fs::read_to_string(root().join("projects.json")).map_err(|e| format!("projects.json: {e}"))?;
    let projects = serde_json::from_str::<Projects>(&text).map_err(|e| format!("projects.json: {e}"))?.projects;
    let template = std::fs::read_to_string(root().join("README.template.md")).map_err(|e| format!("README.template.md: {e}"))?;
    if template.matches(PLACEHOLDER).count() != 1 {
        return Err(format!("README.template.md must contain exactly one {PLACEHOLDER} marker"));
    }

    let metas = collect_metadata(&github, &projects);
    let charts = write_charts(&projects, &metas)?;
    let generated = template.replace(PLACEHOLDER, &render_catalog(&projects, &metas, &charts)).trim_end().to_string() + "\n";

    let path = root().join("README.md");
    if check {
        if std::fs::read_to_string(&path).unwrap_or_default() != generated {
            eprintln!("README.md is out of date; run `cargo run --quiet -- readme`.");
            return Ok(ExitCode::FAILURE);
        }
        println!("README.md is current.");
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::write(&path, generated).map_err(|e| format!("README.md: {e}"))?;
    let count = charts.iter().flatten().count();
    println!("Generated README.md with {} projects and {count} charts.", projects.len());
    Ok(ExitCode::SUCCESS)
}
