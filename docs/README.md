# infrastore Documentation

This directory contains the source files for the infrastore user and developer documentation, built
with [mdBook](https://rust-lang.github.io/mdBook/).

## Building the Documentation

### Prerequisites

Install mdBook and the Mermaid preprocessor:

```bash
cargo install mdbook mdbook-mermaid
```

### Build Commands

**Build the documentation:**

```bash
mdbook build
```

Run this from the `docs/` directory. Output is written to `docs/book/`.

**Serve locally with live reload:**

```bash
mdbook serve --open
```

This builds the docs, starts a local server at `http://localhost:3000`, watches for changes, and
rebuilds automatically.

**Clean build artifacts:**

```bash
mdbook clean
```

## Documentation Structure

The documentation follows the [Diataxis](https://diataxis.fr/) framework:

| Category        | Location               | Purpose                                           |
| --------------- | ---------------------- | ------------------------------------------------- |
| **Tutorials**   | `src/getting-started/` | Learning-oriented quick starts                    |
| **How-To**      | `src/how-to/`          | Task-oriented integration recipes                 |
| **Explanation** | `src/explanation/`     | Understanding-oriented architecture and design    |
| **Guides**      | `src/guides/`          | Language-specific developer guides                |
| **Reference**   | `src/reference/`       | Information-oriented API and file-format listings |

## Editing Documentation

1. Edit the Markdown files under `src/`.
2. When adding a page, add an entry to `src/SUMMARY.md`.
3. Preview with `mdbook serve`.
4. Markdown is wrapped at 100 characters to match the rest of the repository.

## Publishing

The site is published to GitHub Pages from the `gh-pages` branch at
`https://natlabrockies.github.io/time-series-store/`.

`.github/workflows/docs.yml` runs on every push to `main` that touches `docs/`, builds the book with
`site-url` rewritten to `/time-series-store/latest/`, verifies internal and external links, and
commits the result to `gh-pages` under `latest/`. The branch root holds `index.html` (a copy of
`redirect.html`), which reads `versions.json` and forwards visitors to the newest release, or to
`latest/` when no release has been published.

The layout is versioned so tagged releases can be deployed alongside `latest/` in their own
directories, with `version-selector.js` rendering a picker in the header from `versions.json`. Until
a release workflow adds those entries, the selector offers `latest (main)` only.

Link checking also runs on pull requests via `.github/workflows/lint.yml`, restricted to internal
links so PRs are not gated on third-party sites being reachable.

## Theme Assets

The `custom.css`, `pagetoc.css`, `pagetoc.js`, `mermaid.min.js`, `mermaid-init.js`,
`version-selector.css`, and `version-selector.js` files provide the page table of contents,
wide-table layout, Mermaid diagram rendering, and the version picker. They are shared with the
look-and-feel of sibling NatLabRockies documentation sites.
