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
- **旧槽自动下线(由流量入口接管生命周期)**:`deploy` 切完并复检通过后**不停旧 slot**,旧 slot 继续运行(无 drain 等待)。切换后旧 slot 由 front-proxy 统一管理——连续无流量超过 `LLM_PROVIDER_ROUTER_IDLE_SHUTDOWN_SECONDS`(默认 900s=15min)后自动 `systemctl --user stop llm-provider-router-backend@{slot}.service` 下线;重新切回该 slot 时入口自动拉起。配置 `LLM_PROVIDER_ROUTER_IDLE_SHUTDOWN_SECONDS=0` 可禁用自动下线(回到永久常驻)。需手动停某 slot 仍可 `systemctl --user stop llm-provider-router-backend@{slot}.service`(入口不会自动拉起非活跃槽)。查看槽生命周期状态:`python3 bin/hot-deploy-router.py status`(含 idle_for / last_action)或 `curl http://127.0.0.1:8789/_proxy/health` 的 `slot_management` 字段。
- **front-proxy 需先于 backend 升级**:槽生命周期管理由 front-proxy 执行,升级本功能后必须先重启 front-proxy 服务(加载新 front-proxy 二进制)再走 deploy,否则旧 front-proxy 无管理能力、旧槽不会自动下线。
- **禁止按进程名 pkill/kill**:blue/green 槽与测试实例的命令行完全相同(都是 `target/release/llm-provider-router backend`),`pkill -f` 按名匹配会误杀生产槽(2026-09-04 事故:`pkill -o` 把存活 2d16h 的 blue 槽杀掉,存量流全部中断,auto-heal 23s 后才拉回)。清理指定槽用 `systemctl --user stop llm-provider-router-backend@{slot}`;清理测试实例用 `fuser -k <port>/tcp` 按端口定位。
- **优雅退出保障(2026-09-04 起)**:backend/front-proxy 收到 SIGTERM 后停止接新连接并等存量流跑完(85s 看门狗,落在 systemd 默认 TimeoutStopSec 90s 内);idle 下线决策要求该槽在途请求数为 0,避免下线切断切换前遗留的长流;front-proxy 对连接层失败的目标槽会立即 `ensure_running` 并短等重试一次,双槽全停时的 503 空窗从 ~30s 缩到亚秒级。
