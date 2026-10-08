# systemd EnvironmentFile 死引用导致槽无法 spawn（Result 'resources'）

用于：排查 llm-provider-router blue/green 槽启动失败 "Job failed because of unavailable resources"、journal 报 "Failed to load environment files" 的问题；以及改 unit 模板后的正确同步步骤。

触发词：unavailable resources、Failed to load environment files、Failed to spawn、backend@blue 启动失败、EnvironmentFile、agent-secrets.env、daemon-reload、reset-failed。

不适用：端口占用导致的启动失败（那会看到 bind address already in use）；front-proxy（`llm-provider-router.service`）自身的故障；二进制缺失（"Failed to spawn" 但路径指向 target/release 时先确认 cargo build --release 是否跑过）。

---

## 现象

- `bin/hot-deploy-router.py deploy` 拉起非活跃槽时报：
  `Job for llm-provider-router-backend@blue.service failed because of unavailable resources or another system error`，脚本以非零退出。
- `journalctl --user -u llm-provider-router-backend@blue.service`：
  ```
  Failed to load environment files: No such file or directory
  Failed to spawn 'start' task: No such file or directory
  Failed with result 'resources'
  ```
- 服务进入 `activating (auto-restart)` 循环，`Mem peak: 0B`（进程根本没起来）。
- **陷阱**：在跑的槽完全正常——EnvironmentFile 只在启动时读取，运行中的 green 槽毫无异常；若此时回滚（重启 green）会**同样失败**，双槽全灭。

## 根因

unit 模板（`systemd/llm-provider-router-backend@.service`，安装副本在 `~/.config/systemd/user/`）引用了三个已不存在的文件：

```
EnvironmentFile=/home/hevin/.config/opencode/agent-secrets.env
EnvironmentFile=/home/hevin/.config/opencode/agent-internal.env
EnvironmentFile=/home/hevin/.config/opencode/agent-config.env
```

`~/.config/opencode/` 整个目录已废弃，env 由 secret-vault 统一合并生成 `~/.config/environment.d/agent-env.conf`（含全部 `AGENT_AI_*` provider keys）。systemd 对缺失的 EnvironmentFile **直接拒绝 spawn**（不是告警）。

## 修复（2026-10-02）

1. 从 `systemd/llm-provider-router-backend@.service` 删除三个死引用（保留 agent-env.conf 行），注释说明历史与原因。
2. 同步安装副本（两份是**独立拷贝**，不是软链接，改 repo 不会自动生效）：
   `cp systemd/llm-provider-router-backend@.service ~/.config/systemd/user/`
3. `systemctl --user daemon-reload && systemctl --user reset-failed llm-provider-router-backend@blue.service`
4. 重试 `bin/hot-deploy-router.py deploy`。

## 排查线索

- **journal**：`journalctl --user -u 'llm-provider-router-backend@*' -n 50`，关键字 "Failed to load environment files"。
- **存在性检查**：把 unit 里每条 `EnvironmentFile=` 路径逐个 `[ -f ]`（本次三个全缺）。
- **双份同步**：repo `systemd/` 与 `~/.config/systemd/user/` 内容需一致；diff 校验后再 daemon-reload。
- **相关记录**：ark 订阅失效 key 的冻结机制见 `pitfall-ark-subscription-400-key-freeze.md`。
