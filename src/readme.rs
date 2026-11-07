//! Generates README.md and the activity charts from projects.json and current GitHub metadata.
//!
//! Each project gets a card: its icon, a plastic star badge for its tier, a bar chart of commits
//! per day over the last 28 days and its latest release. The badge art is rendered once by
//! tools/badges; this only assembles charts from those sprites, so it needs no GPU.
//!
//! Charts are written to build/charts and published by the workflow to the `assets` branch.
//! Without a GITHUB_TOKEN (GraphQL needs one) cards are generated without charts.

use crate::github::{self, GitHub, Repository, Target};
use crate::{read, read_json, root, token};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, Utc};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{imageops, ExtendedColorType, GenericImageView, ImageEncoder, RgbaImage};
use indexmap::IndexMap;
use rayon::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::process::ExitCode;

const PLACEHOLDER: &str = "<!-- PROJECTS -->";
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

/// What a card shows from GitHub.
#[derive(Default)]
struct Meta {
    stars: Option<u64>,
    /// Commits per day, oldest first.
    days: Option<Vec<u64>>,
    /// The latest release's tag.
    release: Option<String>,
}

struct Card<'a> {
    project: &'a Project,
    meta: Meta,
    chart: Option<String>,
}

// ------------------------------------------------------------------ metadata

/// Exact commits per UTC day for the last DAYS days, oldest first, in one GraphQL query.
fn daily_commits(github: &GitHub, target: &Target, branch: &str) -> Result<Vec<u64>> {
    #[derive(Deserialize)]
    struct Data {
        repository: Repo,
    }
    #[derive(Deserialize)]
    struct Repo {
        object: Option<HashMap<String, History>>,
    }
    #[derive(Deserialize)]
    struct History {
        #[serde(rename = "totalCount")]
        total_count: u64,
    }

    let quote = |text: &str| serde_json::to_string(text).expect("strings serialize");
    let iso = |time: DateTime<Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let end = Utc::now().date_naive().and_hms_opt(0, 0, 0).expect("midnight exists").and_utc() + Duration::days(1);
    let path = target.path.as_deref().map(|p| format!(", path: {}", quote(p))).unwrap_or_default();
    let days: Vec<String> = (0..DAYS)
        .map(|day| {
            let until = end - Duration::days((DAYS - 1 - day) as i64);
            format!(r#"d{day}: history(since: "{}", until: "{}"{path}) {{ totalCount }}"#, iso(until - Duration::days(1)), iso(until))
        })
        .collect();
    let (owner, name) = (target.owner(), target.repository.split('/').nth(1).unwrap_or_default());
    let query = format!(
        "{{ repository(owner: {}, name: {}) {{ object(expression: {}) {{ ... on Commit {{ {} }} }} }} }}",
        quote(owner),
        quote(name),
        quote(branch),
        days.join(" ")
    );
    let counts = github.graphql::<Data>(&query)?.repository.object.with_context(|| format!("no commit at {branch}"))?;
    (0..DAYS).map(|day| counts.get(&format!("d{day}")).map(|h| h.total_count).context("missing day")).collect()
}

fn latest_release(github: &GitHub, target: &Target) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let (owner, name) = target.repository.split_once('/').unwrap_or_default();
    let release = github::optional(github.get::<Release>(&GitHub::url(&["repos", owner, name, "releases", "latest"], &[])))?;
    Ok(release.map(|r| r.tag_name))
}

/// One project's card data, and any warnings about what couldn't be fetched.
fn fetch(github: &GitHub, project: &Project, repos: &HashMap<String, Repository>) -> (Meta, Vec<String>) {
    let mut warnings = vec![];
    let Some(target) = github::target(&project.url) else { return (Meta::default(), warnings) };
    let Some(repo) = repos.get(&target.repository) else { return (Meta::default(), warnings) };
    let branch = target.branch.as_deref().unwrap_or(&repo.default_branch);

    let release = latest_release(github, &target).unwrap_or_else(|e| {
        warnings.push(format!("{} release: {e:#}", target.label()));
        None
    });
    let days = match github.has_token().then(|| daily_commits(github, &target, branch)) {
        Some(Ok(days)) => Some(days),
        Some(Err(e)) => {
            warnings.push(format!("{} activity: {e:#}", target.label()));
            None
        }
        None => None,
    };
    (Meta { stars: Some(repo.stargazers_count), days, release }, warnings)
}

fn collect_metadata(github: &GitHub, projects: &[Project]) -> Vec<Meta> {
    let names: BTreeSet<String> = projects.iter().filter_map(|p| github::target(&p.url)).map(|t| t.repository).collect();
    let fetched: Vec<(String, Result<Repository>)> = names.into_par_iter().map(|name| (name.clone(), github.repository(&name))).collect();

    let mut warnings = vec![];
    let mut repos = HashMap::new();
    for (name, result) in fetched {
        match result {
            Ok(repo) => _ = repos.insert(name, repo),
            Err(e) => warnings.push(format!("{name}: {e:#}")),
        }
    }
    let (metas, more): (Vec<Meta>, Vec<Vec<String>>) = projects.par_iter().map(|p| fetch(github, p, &repos)).unzip();

    warnings.extend(more.into_iter().flatten());
    warnings.sort();
    warnings.dedup();
    for warning in warnings {
        eprintln!("warning: GitHub metadata unavailable for {warning}");
    }
    metas
}

// ------------------------------------------------------------------ charts

/// assets/bars/bars.json: pixel rects (x, y, w, h) keyed by bar height in half points, or "stub".
#[derive(Deserialize)]
struct SpriteIndex {
    scale: f64,
    sprites: HashMap<String, [u32; 4]>,
}

struct Sprites {
    sheet: RgbaImage,
    index: SpriteIndex,
}

fn load_sprites() -> Result<Sprites> {
    let sheet = image::open(root().join("assets/bars/bars.png")).context("reading assets/bars/bars.png")?.into_rgba8();
    Ok(Sprites { sheet, index: read_json("assets/bars/bars.json")? })
}

fn bar_key(commits: u64) -> String {
    if commits == 0 {
        return "stub".into();
    }
    let level = ((commits as f64).ln_1p() / BAR_FULL_COMMITS.ln_1p()).min(1.0);
    (((BAR_MIN_PT + (BAR_MAX_PT - BAR_MIN_PT) * level) * 2.0).round() as u32).to_string()
}

/// One PNG: a bar per day, standing on the bottom edge.
fn render_chart(days: &[u64], sprites: &Sprites) -> Result<Vec<u8>> {
    let SpriteIndex { scale, sprites: rects } = &sprites.index;
    let px = |pt: f64| (pt * scale).round() as u32;
    let stub = rects.get("stub").context("bars.json has no stub")?;
    let (width, height) = (px(BAR_PITCH_PT * (DAYS - 1) as f64) + stub[2], px(BAR_MAX_PT));
    let mut chart = RgbaImage::new(width, height);
    for (day, &commits) in days.iter().enumerate() {
        let key = bar_key(commits);
        let &[x, y, w, h] = rects.get(&key).with_context(|| format!("bars.json has no {key} sprite"))?;
        let bar = sprites.sheet.view(x, y, w, h);
        imageops::replace(&mut chart, &*bar, px(day as f64 * BAR_PITCH_PT).into(), (height - h).into());
    }
    let mut png = vec![];
    PngEncoder::new_with_quality(&mut png, CompressionType::Best, FilterType::Adaptive).write_image(&chart, width, height, ExtendedColorType::Rgba8)?;
    Ok(png)
}

/// Writes one chart per project with activity; returns each chart's URL with a content hash.
fn write_charts(projects: &[Project], metas: &[Meta]) -> Result<Vec<Option<String>>> {
    let sprites = load_sprites()?;
    let dir = root().join("build/charts");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context("clearing build/charts")?;
    }
    std::fs::create_dir_all(&dir).context("creating build/charts")?;
    projects
        .iter()
        .zip(metas)
        .map(|(project, meta)| {
            let Some(days) = &meta.days else { return Ok(None) };
            let png = render_chart(days, &sprites)?;
            let name = format!("{}.png", slug::slugify(&project.name));
            std::fs::write(dir.join(&name), &png).with_context(|| format!("writing build/charts/{name}"))?;
            let hash = format!("{:x}", Sha256::digest(&png));
            Ok(Some(format!("{ASSETS_URL}/charts/{name}?v={}", &hash[..12])))
        })
        .collect()
}

// ------------------------------------------------------------------ cards

fn escape(text: &str) -> Cow<'_, str> {
    html_escape::encode_quoted_attribute(text)
}

/// 999, 1k, 5.5k, 12.3k, 120k, 1.2m.
fn compact(value: u64) -> String {
    let (number, suffix) = match value {
        0..1_000 => return value.to_string(),
        1_000..1_000_000 => (value as f64 / 1e3, "k"),
        _ => (value as f64 / 1e6, "m"),
    };
    let text = format!("{number:.*}", usize::from(number < 100.0));
    format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
}

fn icon_url(project: &Project) -> String {
    match (&project.image, github::target(&project.url)) {
        (Some(image), _) => image.clone(),
        (None, Some(target)) if !project.official() => format!("https://github.com/{}.png?size=128", target.owner()),
        _ => "assets/rune-mark.svg".into(),
    }
}

fn star_tier(stars: u64) -> &'static str {
    STAR_TIERS.iter().rev().find(|(minimum, _)| stars >= *minimum).map_or("gunmetal", |(_, name)| name)
}

fn card(Card { project, meta, chart }: &Card, icon_size: u32) -> String {
    let url = escape(&project.url);
    let mut lines = vec![
        format!(r#"<a href="{url}"><img src="{}" width="{icon_size}" height="{icon_size}" alt=""></a>"#, escape(&icon_url(project))),
        format!(r#"<a href="{url}"><b>{}</b></a>"#, escape(&project.name)),
    ];
    // Official crates share the Rune repository's stars, so they show only their own activity.
    if let (Some(stars), false) = (meta.stars, project.official()) {
        let count = compact(stars);
        lines.push(format!(r#"<img src="assets/stars/star-{}.png" height="32" align="middle" alt="{count} stars"> <b>{count}</b>"#, star_tier(stars)));
    }
    if let Some(chart) = chart {
        lines.push(format!(r#"<img src="{}" height="32" alt="Commits per day over the last {DAYS} days">"#, escape(chart)));
    }
    if let Some(tag) = &meta.release {
        lines.push(format!("<sub><code>{}</code></sub>", escape(tag)));
    }
    lines.push(format!("<sub>{}</sub>", escape(&project.description)));
    format!("<td align=\"center\" valign=\"top\" width=\"33%\">\n{}\n</td>", lines.join("<br>\n"))
}

fn grid(cards: &[&Card], icon_size: u32) -> String {
    let rows: Vec<String> = cards
        .chunks(3)
        .map(|row| format!("<tr>\n{}\n</tr>", row.iter().map(|c| card(c, icon_size)).collect::<Vec<_>>().join("\n")))
        .collect();
    format!("<table>\n{}\n</table>", rows.join("\n"))
}

/// GitHub's anchor for a heading: lowercase, punctuation dropped, spaces to hyphens.
fn anchor(heading: &str) -> String {
    heading.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-')).collect::<String>().replace(' ', "-")
}

fn render_catalog(cards: &[Card]) -> String {
    // Categories and subcategories in the order projects.json lists them.
    let mut catalog: IndexMap<&str, IndexMap<Option<&str>, Vec<&Card>>> = IndexMap::new();
    for card in cards {
        let project = card.project;
        catalog.entry(&project.category).or_default().entry(project.subcategory.as_deref()).or_default().push(card);
    }

    let mut featured: Vec<&Card> = cards.iter().filter(|c| c.project.category == "Apps" && !c.project.official()).collect();
    featured.sort_by_key(|c| Reverse(c.meta.stars.unwrap_or(0)));
    featured.truncate(FEATURED);

    let mut sections = vec!["## Featured".to_string(), grid(&featured, 96)];
    for (category, subcategories) in &catalog {
        sections.push(format!("## {category}"));
        if let Some(entries) = subcategories.get(&None) {
            sections.push(grid(entries, 56));
            continue;
        }
        let nav: Vec<String> = subcategories.keys().flatten().map(|s| format!("[{s}](#{})", anchor(s))).collect();
        sections.push(nav.join(" · "));
        for (subcategory, entries) in subcategories {
            sections.push(format!("### {}", subcategory.unwrap_or_default()));
            sections.push(grid(entries, 56));
        }
    }

    let tiers: Vec<String> =
        STAR_TIERS.iter().map(|&(minimum, name)| if minimum == 0 { format!("{name} under {}", STAR_TIERS[1].0) } else { format!("{name} {}+", compact(minimum)) }).collect();
    sections.push(format!("<sub>Stars: {}. Bars: commits per day over the last {DAYS} days, oldest on the left.</sub>", tiers.join(", ")));
    sections.join("\n\n")
}

pub fn run(check: bool) -> Result<ExitCode> {
    let token = token();
    if token.is_none() {
        eprintln!("warning: no GITHUB_TOKEN; generating without activity charts");
    }
    let github = GitHub::new(token);
    let projects = read_json::<Projects>("projects.json")?.projects;
    let template = read("README.template.md")?;
    ensure!(template.matches(PLACEHOLDER).count() == 1, "README.template.md must contain exactly one {PLACEHOLDER} marker");

    let metas = collect_metadata(&github, &projects);
    let charts = write_charts(&projects, &metas)?;
    let chart_count = charts.iter().flatten().count();
    let cards: Vec<Card> = projects.iter().zip(metas).zip(charts).map(|((project, meta), chart)| Card { project, meta, chart }).collect();
    let generated = template.replace(PLACEHOLDER, &render_catalog(&cards)).trim_end().to_string() + "\n";

    if check {
        if read("README.md").unwrap_or_default() != generated {
            eprintln!("README.md is out of date; run `cargo run --quiet -- readme`.");
            return Ok(ExitCode::FAILURE);
        }
        println!("README.md is current.");
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::write(root().join("README.md"), generated).context("writing README.md")?;
    println!("Generated README.md with {} projects and {chart_count} charts.", projects.len());
    Ok(ExitCode::SUCCESS)
}
