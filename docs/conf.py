# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Configuration file for the Sphinx documentation builder.
#
# For the full list of built-in configuration values, see the documentation:
# https://www.sphinx-doc.org/en/master/usage/configuration.html

# -- Project information -----------------------------------------------------
project = "usd-caches"
copyright = "2026, NVIDIA Corporation"
author = "NVIDIA Corporation"

REPO_URL = "https://github.com/NVIDIA-Omniverse/usd-caches"

# -- General configuration ---------------------------------------------------
extensions = [
    "myst_parser",
    "sphinx.ext.extlinks",
    "sphinx_copybutton",
    "sphinx_design",
]

source_suffix = {
    ".rst": "restructuredtext",
    ".md": "markdown",
}

myst_enable_extensions = ["colon_fence", "deflist"]
myst_heading_anchors = 3

# {src}`path` links to a file or directory in the repository on GitHub; the
# component source, skills, and charts live outside this Sphinx source tree.
extlinks = {
    "src": (f"{REPO_URL}/blob/main/%s", "%s"),
}


# README.md is contributor-facing build instructions, not published documentation.
exclude_patterns = ["_build", "Thumbs.db", ".DS_Store", "README.md"]

# -- Options for HTML output -------------------------------------------------
html_theme = "nvidia_sphinx_theme"

html_theme_options = {
    "collapse_navigation": False,
    "navigation_depth": 4,
    "pygments_light_style": "sas",
    "pygments_dark_style": "github-dark",
    "icon_links": [
        {
            "name": "GitHub",
            "url": REPO_URL,
            "icon": "fa-brands fa-github",
            "type": "fontawesome",
        },
    ],
}
