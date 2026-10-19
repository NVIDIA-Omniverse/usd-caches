# Related experts

usd-caches is one part of Omniverse Content Services. Developers often arrive here with a question that is partly about another repository. Answer the cache part yourself, then send the rest to the expert that owns it, with a specific question.

## How to hand off

1. **Find the boundary.** Say which part of the question is about caches and which part belongs to another repo.
2. **Answer the cache part** from this skill's references.
3. **Name the expert and the question.** Give the repo, the skill to load, and the exact question to ask, for example: "Ask the ovstorage expert: does the `s3` backend accept a custom endpoint, so reads can go through OVCC?"
4. **Use the checkout if it's there.** If the other repo is checked out next to this one (the directory name may end in `-internal`), read its `AGENTS.md` and load the named skill before answering. If it isn't, link the repository and say the answer needs that repo.
5. **Don't answer for another repo from memory.** The summaries below are for routing only. Facts about another repo's behavior, configuration, or APIs come from its own expert or source.

If you can't tell which repo a developer is working in, ask: "Are you calling storage through ovstorage, the Omniverse Client Library or Kit, or the Content Services APIs?"

## usd-content-workspace

- **Repository:** [NVIDIA-Omniverse/usd-content-workspace](https://github.com/NVIDIA-Omniverse/usd-content-workspace).
- **Expert:** `usd-content-workspace-expert` in `skills/usd-content-workspace-expert/SKILL.md`. Also `local-process-stack` for running a stack locally.
- **Owns:** the Content Services APIs and services: Storage, Metadata, Discovery, Notifications, Permissions, User Info, and Thumbnails, plus the external Live Edit Service. It covers building, deploying, securing, and operating them (NGC containers, Helm charts, Kubernetes), calling their versioned gRPC and REST APIs, custom adapters, conformance tests, and connecting Kit, the Client Library, or Navigator to those services.
- **Hand off when:** the question is about a Content Services service (for example, Storage Service authorization, Discovery search, or Permissions), deploying the full stack, or how Kit or Navigator connects to those services.
- **Questions to ask it:** "Which storage endpoint does a Content Services stack expose to Kit, and is it S3-compatible?" "Does the Storage Service redirect reads to object storage, and to which hostnames?" The answers decide whether OVCC can sit in front of those reads.

## ovstorage

- **Repository:** [NVIDIA-Omniverse/ovstorage](https://github.com/NVIDIA-Omniverse/ovstorage).
- **Expert:** start with the repo's `AGENTS.md`, which routes by task, then the task skills in `skills/`:
  - `ovstorage-user-*` for using the library: getting started, choosing a backend, authenticating, reading, writing, deleting, listing, materializing, handling errors, and connecting through MCP.
  - `ovstorage-operator-*` for running the broker: deploying it, configuring policy, monitoring, and debugging.
  - `ovstorage-contributor-*` for changing the code: plugins, the authorization layer, C headers, and integration tests.
  - The broader `ovstorage-expert` ships with the ovstorage USD resolver checkout and also covers the resolver.
- **Owns:** a portable storage abstraction with a stable plugin ABI. It has a Rust core, a CLI, an MCP server, C, C++, and Python bindings, backend plugins (file, HTTP, S3, GCS, Azure, Hugging Face, Nucleus, the Omniverse Storage Service, and more), wrapper layers (router, retry, `byte_cache`, `metadata_cache`, `hub_cache`), a gRPC broker with a REST gateway, an authorization layer, and a TOML policy engine. It also includes the USD resolver.
- **Hand off when:** the question is about ovstorage stacks, backends, credentials, the broker, the resolver, or ovstorage's own embedded caches.
- **What this skill covers:** the Hub side of ovstorage's `hub_cache` layer, and the OVCC side of ovstorage's `[ovstorage.content_cache]` forward-proxy path. See [integrations.md](integrations.md#ovstorage-hub_cache-layer).
- **Questions to ask it:** "Which read API sets `if_match` in our host language, so OVCC can store the read?" "What happens to reads when the content cache is unreachable?" "Which `hub_cache` key scheme should this host use?"

## ovpackage

- **Repository:** [NVIDIA-Omniverse/ovpackage](https://github.com/NVIDIA-Omniverse/ovpackage).
- **Expert:** `.agents/skills/ovpackage-cli` (CLI) and `.agents/skills/ovpackage-api` (async Python API).
- **Owns:** versioned asset packages. It creates, installs, updates, mirrors, exports, imports, freezes, catalogs, diffs, and checks them across S3, Azure, GCS, Nucleus, and local storage, and covers `.wrapp` files, asset publishing, and reproducible builds.
- **Hand off when:** the question is about packaging, versioning, publishing, or mirroring assets, or copying a package to another region.
- **How it relates to caches:** ovpackage moves or copies whole packages ahead of time; caches keep recently read content close on demand. A multi-region team might mirror packages with ovpackage instead of, or as well as, running OVCC in each region. That choice depends on how often content changes and who controls the copies.
- **Questions to ask it:** "Can ovpackage mirror a package to a bucket in the other region, and how is the mirror kept current?"

## ovlive

- **Repository:** [NVIDIA-Omniverse/ovlive](https://github.com/NVIDIA-Omniverse/ovlive). Expert: `skills/ovlive-expert`.
- **Owns:** live OpenUSD collaboration. This covers the `omni_usd_live` runtime and its bindings, the `omni.kit.live_services` Kit extension, and the Live Edit service (gRPC, backed by Valkey).
- **Hand off when:** the question is about live sessions or Live Edit. Nothing in this repo describes how caches interact with live layers.
