# Building the Documentation

This directory contains the Sphinx source for the usd-caches documentation. Back to the [usd-caches README](../README.md).

## Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| [uv](https://docs.astral.sh/uv/getting-started/installation/) | ≥ 0.4 | `curl -LsSf https://astral.sh/uv/install.sh \| sh` (Linux/macOS) or `winget install astral-sh.uv` (Windows) |
| Python | ≥ 3.11 | provisioned automatically by `uv` |

Sphinx and the other documentation dependencies (MyST, the NVIDIA theme, and so on) are
declared in `requirements.txt` in this directory and are installed automatically by `uv`.

## Building on Linux and macOS

```bash
cd docs
make html
```

## Building on Windows

```bat
cd docs
make.bat html
```

To drive Sphinx directly:

```bash
cd docs
uv run --no-project --python ">=3.11" --with-requirements requirements.txt sphinx-build -M html . _build
```

## Viewing the output

```bash
uv run python -m http.server 8000 -d _build/html
```

Then open <http://localhost:8000/> in a browser.

## Cleaning build artifacts

```bash
make clean
```

## Layout

- `index.rst` — landing page and table of contents.
- `getting_started/` — choosing caches, and getting started with each cache.
- `overview/` — the component map and where usd-caches fits in Omniverse Content Services.
- `agents.md` — the AI coding agent skill.

Pages link to repository files with the `{src}` role, which points at the public GitHub repository.
