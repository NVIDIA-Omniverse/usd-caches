# Makefile for Sphinx documentation

SPHINXOPTS    ?=
SPHINXBUILD   ?= sphinx-build
SOURCEDIR     = .
BUILDDIR      = _build

DOCS_DIR      = $(abspath .)

# Sphinx >= 8.2 needs Python >= 3.11. See requirements.txt.
UV            ?= uv
UVRUN         = $(UV) run --no-project --python '>=3.11' --with-requirements $(DOCS_DIR)/requirements.txt

# Put it first so that "make" without argument is like "make help".
help:
	@$(UVRUN) $(SPHINXBUILD) -M help "$(DOCS_DIR)" "$(DOCS_DIR)/$(BUILDDIR)" $(SPHINXOPTS) $(O)

.PHONY: help Makefile clean html

clean:
	@rm -rf $(BUILDDIR)

html:
	$(UVRUN) $(SPHINXBUILD) -M html "$(DOCS_DIR)" "$(DOCS_DIR)/$(BUILDDIR)" $(SPHINXOPTS) $(O)
