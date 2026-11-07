---
name: awesome-rune-projects
description: Add or update projects in awesome-rune, including categories, URLs, images, sorting, schema validation, and generated README checks. Use for any project-list maintenance in this repository.
---

# Awesome Rune Projects

Edit `projects.json`; never include `README.md` changes in a pull request.

A weekly workflow keeps a `new-projects` issue listing repositories whose `Cargo.toml` depends on Rune but that are in neither `projects.json` nor `ignored.json`. Work through it with `GITHUB_TOKEN=$(gh auth token) cargo run --quiet -- discover`: add each candidate that is a real, documented Rune project, and add the rest to `ignored.json` with a one-line `reason` (a spike, a fork, no README, Rune only in a comparison).

For each project:

- Confirm it actually uses Rune (`github.com/anomalyco/rune` in a `Cargo.toml`), not only mentions it.
- Use its canonical `https://` URL. For a Rune app inside a larger repository, link the subdirectory with a `/tree/<branch>/<path>` URL so activity tracks that path.
- GitHub URLs automatically receive stars and activity; other hosts show `N/A`.
- Choose a schema-defined `category`.
- Apps must have a schema-defined `subcategory`; other categories must not.
- `image` is optional and must be an HTTPS URL. Prefer a stable repository-hosted square icon over a screenshot or badge.
- Keep descriptions concise and preserve existing style.

Run, in order:

```sh
cargo run --quiet -- validate
cargo run --quiet -- sort
cargo run --quiet -- sort --check
GITHUB_TOKEN=$(gh auth token) cargo run --quiet -- readme
```

Review the generated `README.md` locally, confirm metadata changes are expected, then run `git restore README.md` before committing. Activity charts need the token (GraphQL); they are written to the ignored `build/charts` and published by the workflow to the `assets` branch. The star and bar art in `assets/stars` and `assets/bars` comes from `tools/badges/render.sh`; re-render it only when the look changes. The post-merge workflow updates it. Confirm sorting changed only placement and GitHub redirects use canonical URLs. Never commit `.env` or `target/`.
