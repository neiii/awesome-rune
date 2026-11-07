#!/usr/bin/env -S uv run --script
"""Generate README.md and the activity charts from projects.json and current GitHub metadata.

Each project gets a card: its icon, a plastic star badge for its tier, a bar chart of commits per
day over the last 28 days, and a line with active days, its latest release and how far its pinned
Rune revision trails `dev`. The badge art is rendered once by tools/badges; this script only
assembles charts from those sprites, so it needs no GPU.

Charts are written to build/charts and published by the workflow to the `assets` branch. Without
a GITHUB_TOKEN (GraphQL needs one) cards are generated without charts.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
import os
import re
import sys
from datetime import datetime, timedelta, timezone
from html import escape
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import quote, unquote, urlencode, urlparse
from urllib.request import Request, urlopen

from dotenv import load_dotenv
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
PROJECTS_PATH = ROOT / "projects.json"
TEMPLATE_PATH = ROOT / "README.template.md"
README_PATH = ROOT / "README.md"
CHARTS_PATH = ROOT / "build" / "charts"
SPRITES_PATH = ROOT / "assets" / "bars"
PLACEHOLDER = "<!-- PROJECTS -->"
API_ROOT = "https://api.github.com"
RUNE_REPO = "anomalyco/rune"
RUNE_URL = f"https://github.com/{RUNE_REPO}"
# Published by the update-readme workflow; the query string busts GitHub's image cache.
ASSETS_URL = "https://raw.githubusercontent.com/neiii/awesome-rune/assets"

DAYS = 28
FEATURED = 3
# Star tiers, lowest first: (minimum stars, badge).
STAR_TIERS = [(0, "gunmetal"), (10, "rose"), (100, "silver"), (1000, "gold")]
# Bars: one per day, 4 pt wide on a 6 pt pitch, 6..32 pt tall on a log scale where 60 commits
# reach the top; a quiet day is the 4 pt stub. Heights round to the sprites' half-point steps.
BAR_PITCH_PT = 6.0
BAR_MIN_PT, BAR_MAX_PT = 6.0, 32.0
BAR_FULL_COMMITS = 60


def github_get(path: str, token: str | None, params: dict[str, str] | None = None, raw: bool = False) -> Any:
    url = f"{API_ROOT}{path}"
    if params:
        url = f"{url}?{urlencode(params)}"
    headers = {
        "Accept": "application/vnd.github.raw" if raw else "application/vnd.github+json",
        "User-Agent": "awesome-rune-readme-generator",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = Request(url, headers=headers)
    with urlopen(request, timeout=30) as response:
        return response.read().decode("utf-8") if raw else json.load(response)


def github_graphql(query: str, token: str) -> Any:
    request = Request(
        f"{API_ROOT}/graphql",
        data=json.dumps({"query": query}).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "User-Agent": "awesome-rune-readme-generator",
        },
    )
    with urlopen(request, timeout=60) as response:
        body = json.load(response)
    if body.get("errors"):
        raise ValueError(body["errors"][0].get("message", "GraphQL error"))
    return body["data"]


def parse_date(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def compact_stars(value: int | None) -> str:
    if value is None:
        return "—"
    if value < 1_000:
        return str(value)
    if value < 1_000_000:
        number, suffix = value / 1_000, "k"
    else:
        number, suffix = value / 1_000_000, "m"
    precision = 1 if number < 100 else 0
    return f"{number:.{precision}f}".rstrip("0").rstrip(".") + suffix


def github_target(url: str) -> tuple[str, str | None, str | None] | None:
    """(owner/name, branch from a /tree/ URL, path inside the repository)."""
    parsed = urlparse(url)
    if parsed.hostname is None or parsed.hostname.lower() not in {"github.com", "www.github.com"}:
        return None

    parts = [unquote(part) for part in parsed.path.split("/") if part]
    if len(parts) < 2:
        return None

    owner, repository_name = parts[0], parts[1].removesuffix(".git")
    if not owner or not repository_name:
        return None

    tree = len(parts) >= 4 and parts[2] == "tree"
    branch = parts[3] if tree else None
    path = "/".join(parts[4:]) if tree and len(parts) >= 5 else None
    return f"{owner}/{repository_name}", branch, path


def is_official(project: dict[str, Any]) -> bool:
    return project["url"].startswith(f"{RUNE_URL}/")


def slug(name: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


# ------------------------------------------------------------------ metadata


def daily_commits(repository: str, branch: str, path: str | None, token: str) -> list[int]:
    """Exact commits per UTC day for the last DAYS days, oldest first, in one GraphQL query."""
    owner, name = repository.split("/")
    end = datetime.now(timezone.utc).replace(hour=0, minute=0, second=0, microsecond=0) + timedelta(days=1)
    path_arg = f", path: {json.dumps(path)}" if path else ""
    fields = []
    for day in range(DAYS):
        until = end - timedelta(days=DAYS - 1 - day)
        since = until - timedelta(days=1)
        fields.append(
            f'd{day}: history(since: "{since:%Y-%m-%dT%H:%M:%SZ}", until: "{until:%Y-%m-%dT%H:%M:%SZ}"{path_arg}) '
            "{ totalCount }"
        )
    query = (
        f"{{ repository(owner: {json.dumps(owner)}, name: {json.dumps(name)}) "
        f"{{ object(expression: {json.dumps(branch)}) {{ ... on Commit {{ {' '.join(fields)} }} }} }} }}"
    )
    commit = github_graphql(query, token)["repository"]["object"]
    if commit is None:
        raise ValueError(f"no commit at {branch}")
    return [commit[f"d{day}"]["totalCount"] for day in range(DAYS)]


def latest_release(repository: str, token: str | None) -> dict[str, Any] | None:
    try:
        release = github_get(f"/repos/{repository}/releases/latest", token)
    except HTTPError as error:
        if error.code == 404:
            return None
        raise
    return {"tag": release["tag_name"], "date": parse_date(release.get("published_at"))}


def rune_pin(repository: str, branch: str, path: str | None, token: str | None) -> str | None:
    """The Rune revision the project's Cargo.lock pins, from its own directory or the root."""
    for lock in [f"{path}/Cargo.lock" if path else None, "Cargo.lock"]:
        if lock is None:
            continue
        try:
            text = github_get(f"/repos/{repository}/contents/{quote(lock)}", token, {"ref": branch}, raw=True)
        except HTTPError as error:
            if error.code == 404:
                continue
            raise
        match = re.search(rf"git\+https://github\.com/{RUNE_REPO}[^#\"]*#([0-9a-f]{{7,40}})", text)
        if match:
            return match.group(1)
    return None


def collect_metadata(projects: list[dict[str, Any]], token: str | None) -> dict[int, dict[str, Any]]:
    targets = {id(project): github_target(project["url"]) for project in projects}
    repositories: dict[str, dict[str, Any] | None] = {}
    behind: dict[str, int | None] = {}
    errors: set[str] = set()

    for target in targets.values():
        if target is None or target[0] in repositories:
            continue
        repository = target[0]
        try:
            repositories[repository] = github_get(f"/repos/{repository}", token)
        except (HTTPError, URLError, TimeoutError, OSError, ValueError) as error:
            repositories[repository] = None
            errors.add(f"{repository}: {error}")

    results: dict[int, dict[str, Any]] = {}
    for project in projects:
        result: dict[str, Any] = {"stars": None, "archived": False, "available": False, "days": None, "release": None, "behind": None}
        results[id(project)] = result
        target = targets[id(project)]
        if target is None:
            continue
        repository, branch, path = target
        metadata = repositories.get(repository)
        if metadata is None:
            continue
        branch = branch or metadata.get("default_branch", "main")
        result.update(stars=metadata.get("stargazers_count"), archived=bool(metadata.get("archived")), available=True)

        label = f"{repository}/{path}" if path else repository
        try:
            result["release"] = latest_release(repository, token)
        except (HTTPError, URLError, TimeoutError, OSError, ValueError) as error:
            errors.add(f"{label} release: {error}")
        if token:
            try:
                result["days"] = daily_commits(repository, branch, path, token)
            except (HTTPError, URLError, TimeoutError, OSError, ValueError, KeyError, TypeError) as error:
                errors.add(f"{label} activity: {error}")
        if is_official(project):
            continue
        try:
            pin = rune_pin(repository, branch, path, token)
            if pin and pin not in behind:
                comparison = github_get(f"/repos/{RUNE_REPO}/compare/{pin}...dev", token)
                behind[pin] = comparison.get("ahead_by")
            result["behind"] = behind.get(pin) if pin else None
        except (HTTPError, URLError, TimeoutError, OSError, ValueError) as error:
            errors.add(f"{label} Rune pin: {error}")

    for error in sorted(errors):
        print(f"warning: GitHub metadata unavailable for {error}", file=sys.stderr)
    return results


# ------------------------------------------------------------------ charts


def load_sprites() -> tuple[Image.Image, dict[str, list[int]], float]:
    index = json.loads((SPRITES_PATH / "bars.json").read_text(encoding="utf-8"))
    return Image.open(SPRITES_PATH / "bars.png").convert("RGBA"), index["sprites"], index["scale"]


def bar_key(commits: int) -> str:
    if commits == 0:
        return "stub"
    level = min(math.log1p(commits) / math.log1p(BAR_FULL_COMMITS), 1.0)
    return str(round((BAR_MIN_PT + (BAR_MAX_PT - BAR_MIN_PT) * level) * 2))


def render_chart(days: list[int], sprites: tuple[Image.Image, dict[str, list[int]], float]) -> bytes:
    sheet, index, scale = sprites
    width, height = round(BAR_PITCH_PT * (DAYS - 1) * scale + index["stub"][2]), round(BAR_MAX_PT * scale)
    chart = Image.new("RGBA", (width, height))
    for day, commits in enumerate(days):
        x, y, w, h = index[bar_key(commits)]
        chart.alpha_composite(sheet.crop((x, y, x + w, y + h)), (round(day * BAR_PITCH_PT * scale), height - h))
    out = io.BytesIO()
    chart.save(out, format="PNG", optimize=True)
    return out.getvalue()


def write_charts(projects: list[dict[str, Any]], results: dict[int, dict[str, Any]]) -> dict[int, str]:
    """Writes one chart per project with activity; returns each chart's URL with a content hash."""
    sprites = load_sprites()
    CHARTS_PATH.mkdir(parents=True, exist_ok=True)
    for stale in CHARTS_PATH.glob("*.png"):
        stale.unlink()
    urls: dict[int, str] = {}
    for project in projects:
        days = results[id(project)]["days"]
        if days is None:
            continue
        png = render_chart(days, sprites)
        name = f"{slug(project['name'])}.png"
        (CHARTS_PATH / name).write_bytes(png)
        urls[id(project)] = f"{ASSETS_URL}/charts/{name}?v={hashlib.sha256(png).hexdigest()[:12]}"
    return urls


# ------------------------------------------------------------------ cards


def icon_url(project: dict[str, Any]) -> str:
    if project.get("image"):
        return project["image"]
    if is_official(project):
        return "assets/rune-mark.svg"
    target = github_target(project["url"])
    return f"https://github.com/{target[0].split('/')[0]}.png?size=128" if target else "assets/rune-mark.svg"


def star_tier(stars: int) -> str:
    return next(name for minimum, name in reversed(STAR_TIERS) if stars >= minimum)


def days_ago(date: datetime) -> str:
    days = max(0, (datetime.now(timezone.utc) - date).days)
    return "today" if days == 0 else f"{days}d ago"


def card(project: dict[str, Any], result: dict[str, Any], chart: str | None, icon_size: int) -> str:
    url = escape(project["url"], quote=True)
    lines = [
        f'<a href="{url}"><img src="{escape(icon_url(project), quote=True)}" width="{icon_size}" height="{icon_size}" alt=""></a>',
        f'<a href="{url}"><b>{escape(project["name"])}</b></a>',
    ]
    # Official crates share the Rune repository's stars, so only their own activity is shown.
    if result["stars"] is not None and not is_official(project):
        tier = star_tier(result["stars"])
        lines.append(
            f'<img src="assets/stars/star-{tier}.png" height="32" align="middle" alt="{escape(compact_stars(result["stars"]))} stars"> '
            f"<b>{compact_stars(result['stars'])}</b>"
        )
    if chart:
        lines.append(f'<img src="{escape(chart, quote=True)}" height="32" alt="Commits per day over the last {DAYS} days">')

    facts = []
    if result["days"] is not None:
        facts.append(f"<b>{sum(1 for commits in result['days'] if commits)}</b>/{DAYS} days")
    if result["release"]:
        when = f" {days_ago(result['release']['date'])}" if result["release"]["date"] else ""
        facts.append(f"<code>{escape(result['release']['tag'])}</code>{when}")
    if result["behind"] is not None:
        facts.append(f"Rune pin {result['behind']} behind")
    if result["archived"]:
        facts.append("archived")
    elif not result["available"]:
        facts.append("unavailable")
    if facts:
        lines.append(f"<sub>{' · '.join(facts)}</sub>")
    lines.append(f"<sub>{escape(project['description'])}</sub>")
    return '<td align="center" valign="top" width="33%">\n' + "<br>\n".join(lines) + "\n</td>"


def grid(projects: list[dict[str, Any]], results: dict[int, dict[str, Any]], charts: dict[int, str], icon_size: int = 56) -> str:
    rows = []
    for start in range(0, len(projects), 3):
        cells = [card(project, results[id(project)], charts.get(id(project)), icon_size) for project in projects[start : start + 3]]
        rows.append("<tr>\n" + "\n".join(cells) + "\n</tr>")
    return "<table>\n" + "\n".join(rows) + "\n</table>"


def anchor(heading: str) -> str:
    return re.sub(r"[^a-z0-9 -]", "", heading.lower()).replace(" ", "-")


def group_projects(
    projects: list[dict[str, Any]],
) -> dict[str, dict[str | None, list[dict[str, Any]]]]:
    catalog: dict[str, dict[str | None, list[dict[str, Any]]]] = {}
    for project in projects:
        category = project["category"]
        subcategory = project.get("subcategory")
        catalog.setdefault(category, {}).setdefault(subcategory, []).append(project)
    return catalog


def render_catalog(
    projects: list[dict[str, Any]],
    results: dict[int, dict[str, Any]],
    charts: dict[int, str],
) -> str:
    catalog = group_projects(projects)
    apps = [project for project in projects if project["category"] == "Apps" and not is_official(project)]
    featured = sorted(apps, key=lambda project: -(results[id(project)]["stars"] or 0))[:FEATURED]
    sections = ["## Featured", grid(featured, results, charts, icon_size=96)]
    for category, subcategories in catalog.items():
        sections.append(f"## {category}")
        if None in subcategories:
            sections.append(grid(subcategories[None], results, charts))
            continue
        sections.append(" · ".join(f"[{name}](#{anchor(name)})" for name in subcategories))
        for subcategory, entries in subcategories.items():
            sections.extend([f"### {subcategory}", grid(entries, results, charts)])

    tiers = ", ".join(f"{name} {minimum:,}+" if minimum else f"{name} under {STAR_TIERS[1][0]}" for minimum, name in STAR_TIERS)
    sections.append(
        f"<sub>Stars: {tiers}. Bars: commits per day over the last {DAYS} days, oldest on the left. "
        "Rune pin: how many commits the project's locked Rune revision trails <code>dev</code>.</sub>"
    )
    return "\n\n".join(sections)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Fail if README.md is not current")
    args = parser.parse_args()

    load_dotenv(ROOT / ".env")
    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        print("warning: no GITHUB_TOKEN; generating without activity charts", file=sys.stderr)
    data = json.loads(PROJECTS_PATH.read_text(encoding="utf-8"))
    projects = data["projects"]
    template = TEMPLATE_PATH.read_text(encoding="utf-8")
    if template.count(PLACEHOLDER) != 1:
        raise ValueError(f"{TEMPLATE_PATH.name} must contain exactly one {PLACEHOLDER} marker")

    results = collect_metadata(projects, token)
    charts = write_charts(projects, results)
    generated = template.replace(PLACEHOLDER, render_catalog(projects, results, charts)).rstrip() + "\n"

    if args.check:
        current = README_PATH.read_text(encoding="utf-8") if README_PATH.exists() else ""
        if current != generated:
            print("README.md is out of date; run `uv run scripts/generate_readme.py`.", file=sys.stderr)
            return 1
        print("README.md is current.")
        return 0

    README_PATH.write_text(generated, encoding="utf-8")
    print(f"Generated {README_PATH.relative_to(ROOT)} with {len(projects)} projects and {len(charts)} charts.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
