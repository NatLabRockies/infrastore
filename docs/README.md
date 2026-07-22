# castore Documentation

This directory contains the source files for the castore user and developer documentation, built
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

## Theme Assets

The `custom.css`, `pagetoc.css`, `pagetoc.js`, `mermaid.min.js`, and `mermaid-init.js` files provide
the page table of contents, wide-table layout, and Mermaid diagram rendering. They are shared with
the look-and-feel of sibling NatLabRockies documentation sites.
