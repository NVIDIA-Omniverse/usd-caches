# AI coding agents

The {src}`skills <skills/>` directory contains the `usd-caches-expert` skill, which helps AI coding agents — and people — choose caches for a workflow, and build, deploy, configure, call, and validate each one. Open this repository in an agent that reads {src}`AGENTS.md <AGENTS.md>`, or point your agent at the skill in a checkout of this repository; its references cite the component source, so the skill needs the repository alongside it.

The skill is written to answer questions from other agents as well as from people. An orchestrating agent can ask it which caches a workload needs, how many deployments to run and where, what a configuration field does, or how to verify a cache is working.

Agents working in this repository explain and recommend by default; they build, install, or deploy only when asked.
