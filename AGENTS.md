# AGENTS.md — LLM Provider Router

Personal tool project under `~/Developer/tools/`.

- Keep the implementation lightweight and easy to replace.
- Prefer `uv run ...` for Python commands.
- Do not read or print real API keys. Use environment variables only.
- Follow the `tools/` worktree rule: do not develop directly on `main`; use an isolated git worktree + branch and merge back to `main` after verification (see `~/Developer/tools/AGENTS.md` — Git, Branching & Worktree).

## 字节/头变更检查清单（流式与代理路径改动必过）

> 2026-10-21 两起部署事故的固化教训：单组件测试全过 ≠ 全链路字节正确。
> 改动涉及**响应头、内容类型、SSE/流式转发路径**（含压缩层、守卫、翻译、front-proxy 头处理）时，逐条自查：

1. **字节路径**：这段流经的每一跳（守卫/翻译/压缩层/front-proxy）对字节做了什么？谁复制？谁丢弃？冲刷/转发的字节集合是否恰好等于上游字节集合（不多不少）？
2. **头路径**：每一跳对头做了什么？skip 列表改过吗？`content-encoding`/`content-type`/`cache-control` 是否原样到达客户端？
3. **chunk 边界**：测试是否覆盖行中间截断的多 chunk 场景（`tests/e2e_stream.rs` 字节级回归为准）？
4. **验收口径**：断言“客户端可见结果”（重建后的完整字节流/透传后的头），而不是断言某单组件的内部返回值；存量测试语义可疑时先确认它断的不是 bug 行为。

## Deploy & Switch (部署替换必须验证)

> 切换流量到新版本前,必须先验证新版本能正常工作;未经验证就切换,一旦新版本异常,整个 llm router 会直接瘫痪。

- **标准部署路径是 one-shot `deploy`（2026-10-21 起）**：`deploy` 在拉起→切流之间亚秒级窗口内持锁完成，不受 front-proxy 旧槽 idle 下线竞态影响。两段式 `stage`→`switch` 仅用于需要在切流前长时间人工探测的场景——间隔超过一个检查周期（默认 30s）时，刚拉起的非活跃槽可能被 idle 下线机制误杀（旧槽流量时钟不随重启重置，`idle_for` 会沿用 proxy 启动时刻）；用两段式时必须在 stage 后立即 switch，且结束必 `lock-release --token <T>`（忘释放会阻塞后续部署）。
- **自动化探针（2026-10-21 事故后内置）**：`deploy` 切流前对新槽跑 `precheck`（头语义 + SSE 字节完整性探针，硬失败中止部署），切流后 `verify_after_switch` 在原有 4 路径 JSON 检查上追加同样探针（失败自动回滚）。探针 verdict 分级：结构性损坏（malformed JSON/事件重复/头错误）= fail；上游原因（超时/上游错误/HTTP 4xx 重试后仍失败）= skip（不阻塞）。手动探针：`python3 bin/hot-deploy-router.py precheck --slot <slot>` 或 `--url <base>`；探针模型可用 `--model` 或 `$LLM_PROVIDER_ROUTER_PROBE_MODEL` 覆盖（默认 low-model-auto）。全链路字节级集成测试见 `tests/e2e_stream.rs`（真实二进制 + mock 上游，含行中间截断的多 chunk 场景）。
- **部署互斥锁(2026-10-08 事故:两会话同时部署互相覆盖槽位状态)**:任何部署/切流操作必须持有部署锁 —— `python3 bin/hot-deploy-router.py stage/switch/deploy`(内部自动持锁)或 `python3 bin/deploy_lock.py acquire/release`。锁为 `flock` 文件锁(`~/.local/state/llm-provider-router/deploy.lock`,进程死亡自动释放),acquire 产出 token;锁被持有期间 front-proxy 的 `POST /_proxy/active/{slot}` 只放行携带匹配 `X-Deploy-Lock` 头的请求,否则 409。拿不到锁 = 有其他部署在进行,等待或放弃,绝不并行部署。锁状态:`lock-status` 或 `GET /_proxy/deploy-lock`。
- **先验证,后切换**:部署时先在非活跃 slot(blue/green)拉起新版本,确认 `python3 bin/hot-deploy-router.py status` 显示该 slot `health=ok` 后才允许切换 `active_slot`。
- **切槽自动搬运 delta 缓存(2026-10-27 事故后内置)**:`deploy`/`switch` 在切流前把旧槽 delta 会话缓存 dump→load 给新槽(`/internal/delta-cache/*`,仅本机脚本直连槽端口,front-proxy 封锁该前缀),delta 会话切槽无感。搬运失败仅警告不阻塞——缓存只是优化,客户端扩展(pendingDelta 自愈)保证任何缓存丢失都退化为全量重建,不罢工。
- **前端 dist 是上线资产(2026-10-27 白屏事故)**:更新 dashboard 必须 `cd frontend && VITE_BASE=/llm-router/ pnpm build` 并与二进制同批替换 dist。漏掉 VITE_BASE 产出根路径形态,公网 edge 子路径入口(`/llm-router/*`)资源引用落不到 router,直接白屏。
- **切换后立即复检**:切换完成后必须立刻用 front-proxy 入口验证——`/health` 健康、`/api/config/token-prices`、`/api/config/v2/physical-models`、`/api/router/capabilities` 等核心 API 返回正常 JSON(而非 dashboard HTML fallback)。
- **失败即回滚**:切换后任一检查失败,立即切回原 slot 恢复服务,再排查新版本问题;不得让服务停留在未验证/异常状态。
- **禁止裸替换**:不得直接停旧进程/覆盖可执行文件后立即启动新版本来替换;始终走 hot-deploy 的 blue/green 验证流程。
- **旧槽自动下线(由流量入口接管生命周期)**:`deploy` 切完并复检通过后**不停旧 slot**,旧 slot 继续运行(无 drain 等待)。切换后旧 slot 由 front-proxy 统一管理——连续无流量超过 `LLM_PROVIDER_ROUTER_IDLE_SHUTDOWN_SECONDS`(默认 900s=15min)后自动 `systemctl --user stop llm-provider-router-backend@{slot}.service` 下线;重新切回该 slot 时入口自动拉起。配置 `LLM_PROVIDER_ROUTER_IDLE_SHUTDOWN_SECONDS=0` 可禁用自动下线(回到永久常驻)。需手动停某 slot 仍可 `systemctl --user stop llm-provider-router-backend@{slot}.service`(入口不会自动拉起非活跃槽)。查看槽生命周期状态:`python3 bin/hot-deploy-router.py status`(含 idle_for / last_action)或 `curl http://127.0.0.1:8789/_proxy/health` 的 `slot_management` 字段。
- **front-proxy 需先于 backend 升级**:槽生命周期管理由 front-proxy 执行,升级本功能后必须先重启 front-proxy 服务(加载新 front-proxy 二进制)再走 deploy,否则旧 front-proxy 无管理能力、旧槽不会自动下线。
- **禁止按进程名 pkill/kill**:blue/green 槽与测试实例的命令行完全相同(都是 `target/release/llm-provider-router backend`),`pkill -f` 按名匹配会误杀生产槽(2026-09-04 事故:`pkill -o` 把存活 2d16h 的 blue 槽杀掉,存量流全部中断,auto-heal 23s 后才拉回)。清理指定槽用 `systemctl --user stop llm-provider-router-backend@{slot}`;清理测试实例用 `fuser -k <port>/tcp` 按端口定位。
- **优雅退出保障(2026-09-04 起)**:backend/front-proxy 收到 SIGTERM 后停止接新连接并等存量流跑完(85s 看门狗,落在 systemd 默认 TimeoutStopSec 90s 内);idle 下线决策要求该槽在途请求数为 0,避免下线切断切换前遗留的长流;front-proxy 对连接层失败的目标槽会立即 `ensure_running` 并短等重试一次,双槽全停时的 503 空窗从 ~30s 缩到亚秒级。
