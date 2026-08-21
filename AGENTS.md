# AGENTS.md — LLM Provider Router

Personal tool project under `~/Developer/tools/`.

- Keep the implementation lightweight and easy to replace.
- Prefer `uv run ...` for Python commands.
- Do not read or print real API keys. Use environment variables only.
- Follow the `tools/` worktree rule: do not develop directly on `main`; use an isolated git worktree + branch and merge back to `main` after verification (see `~/Developer/tools/AGENTS.md` — Git, Branching & Worktree).
