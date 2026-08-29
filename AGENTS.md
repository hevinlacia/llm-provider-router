# AGENTS.md — LLM Provider Router

Personal tool project under `~/Developer/tools/`.

- Keep the implementation lightweight and easy to replace.
- Prefer `uv run ...` for Python commands.
- Do not read or print real API keys. Use environment variables only.
- Follow the `tools/` worktree rule: do not develop directly on `main`; use an isolated git worktree + branch and merge back to `main` after verification (see `~/Developer/tools/AGENTS.md` — Git, Branching & Worktree).

## Deploy & Switch (部署替换必须验证)

> 切换流量到新版本前,必须先验证新版本能正常工作;未经验证就切换,一旦新版本异常,整个 llm router 会直接瘫痪。

- **先验证,后切换**:部署时先在非活跃 slot(blue/green)拉起新版本,确认 `python3 bin/hot-deploy-router.py status` 显示该 slot `health=ok` 后才允许切换 `active_slot`。
- **切换后立即复检**:切换完成后必须立刻用 front-proxy 入口验证——`/health` 健康、`/api/config/token-prices`、`/api/config/v2/physical-models`、`/api/router/capabilities` 等核心 API 返回正常 JSON(而非 dashboard HTML fallback)。
- **失败即回滚**:切换后任一检查失败,立即切回原 slot 恢复服务,再排查新版本问题;不得让服务停留在未验证/异常状态。
- **禁止裸替换**:不得直接停旧进程/覆盖可执行文件后立即启动新版本来替换;始终走 hot-deploy 的 blue/green 验证流程。
- **温备常驻,无 drain 等待**:`deploy` 切完并复检通过后**不停旧 slot**,旧 slot 继续常驻作温备。切换成功由复检在 ~5s 内确认;下次发版直接部署该温备 slot(`systemctl restart` 强制加载当前二进制)。需停某 slot 时手动 `systemctl --user stop llm-provider-router-backend@{slot}.service`。
