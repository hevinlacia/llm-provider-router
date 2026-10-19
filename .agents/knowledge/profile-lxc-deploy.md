# llm-provider-router LXC 容器实例部署档案

用于：在全新 PVE LXC 容器安装/重建 llm-provider-router（工作站构建、二进制直拷方案），或排查容器实例与工作站本机实例的行为差异。

触发词：lxc、192.168.6.6、pve 容器、glibc、agent-env.conf、enable-linger、/opt/llm-provider-router、probe_slot、容器部署、迁移 router。

不适用：工作站本机 blue/green 热部署与切流（见 `.agents/skills/llm-router-deploy/SKILL.md`）；路由/key/模型配置本身（见 config/ 与 dashboard）。

## 实例档案（2026-10-17 首次部署）

| 项目 | 值 |
|---|---|
| 容器 | PVE LXC `root@192.168.6.6`，Ubuntu 26.04，x86_64，glibc 2.43 |
| 规格 | 2 核 / 1GB / 8GB rootfs（运行时实测 <100MB RSS，富余） |
| 部署方式 | 工作站 `cargo build --release` 产物直接拷贝，容器内**不编译** |
| 服务 | systemd **user** 单元（root）×2：`llm-provider-router.service`（front-proxy :8789）+ `llm-provider-router-backend@.service`（blue :8790 / green :8791） |
| 监听 | proxy 绑 `[::]` 双栈（2026-10-17 起，IPv4 `192.168.6.6:8789` + IPv6 GUA/ULA 均可达；GUA 动态前缀重拨会变、后缀 EUI-64 稳定；仪表盘/密钥面暴露内网，GUA 全球可路由需确认光猫/主路由入站 deny）；backend 8790/8791 仍 127.0.0.1（仅 proxy 本机可达）；**客户端未切流**，工作站 pi/opencode 仍指本机 8789 |
| 活跃槽 | blue（与工作站一致） |

## 容器内布局

| 内容 | 位置 |
|---|---|
| 二进制 | `/opt/llm-provider-router/llm-provider-router` |
| 配置 JSON（含 api-keys.json） | `/opt/llm-provider-router/config/` |
| 前端 dist | `/opt/llm-provider-router/frontend/dist/` |
| 运维脚本（hot-deploy/deploy_lock/probe_slot） | `/opt/llm-provider-router/bin/` |
| 状态/用量库 + active-backend.json | `/root/.local/state/llm-provider-router/` |
| 密钥 env | `/root/.config/environment.d/agent-env.conf`（600，整文件拷自工作站，含全部 agent secrets） |
| user 单元 | `/root/.config/systemd/user/llm-provider-router{,-backend@}.service`（路径已适配 /opt 与 /root） |

关键前提：backend 以 `WorkingDirectory=/opt/llm-provider-router` 运行，config 路径是相对路径（`config/api-keys.json` 等），前端静态资源也是相对路径 `frontend/dist/`；脚本统一 `systemctl --user` + `~/.local/state` 展开 → 全套按 root + linger 跑，脚本零改动。

## 全新安装步骤

1. 二进制兼容性判定（先拷过去试跑）：`ldd` 无 not found 且 `--help` 出 Usage 即可。若源机构建机 glibc 高于容器，用 `objdump -T <bin> | grep -o 'GLIBC_[0-9.]*' | sort -uV` 看最高要求版本是否 ≤ 容器 glibc。
2. 容器装基础包：`apt-get install -y ca-certificates curl rsync python3`。
3. rsync 产物：二进制、`config/`、`frontend/dist/`、`bin/{hot-deploy-router.py,deploy_lock.py}`、probe 脚本、`~/.local/state/llm-provider-router/{state.sqlite3,usage.sqlite3,active-backend.json}`、`agent-env.conf`（`rsync -a --chmod=600`，密钥文件内容不过上下文）。**不要拷 deploy.lock 和 logs/**。
4. 写入两个 user 单元（拷工作站 `systemd/` 下同名文件，替换路径：repo → `/opt/llm-provider-router`，`/home/hevin` → `/root`；backend 的 `EnvironmentFile`/`LLM_PROVIDER_ROUTER_ENV_FILE` 指向容器 env 路径）。
5. `chown -R root:root /opt/llm-provider-router /root/.local/state/llm-provider-router`。
6. `loginctl enable-linger root` → `XDG_RUNTIME_DIR=/run/user/0 systemctl --user daemon-reload`。
7. `cd /opt/llm-provider-router && XDG_RUNTIME_DIR=/run/user/0 python3 bin/hot-deploy-router.py bootstrap --slot blue --stop-other`。
8. `systemctl --user enable llm-provider-router-backend@blue`（冷启动不依赖 30s auto-heal 空窗）。

## 验证（阶段 2 探针，直连槽端口）

```bash
ssh root@192.168.6.6 'cd /opt/llm-provider-router && python3 bin/probe_slot.py --port 8790 --model low-model-auto --allow-chat-removal --smoke'
```

- `POST /v1/chat/completions` 405 是**预期 WARN**：该端点 2026-10 审计后已从代码删除，工作站生产同二进制同样 405，非容器缺陷。
- `--smoke` 200 = 容器内完整链路（key 解密 → 上游 TLS → 流式响应）打通。
- front-proxy 入口：`/health`、`/api/config/v2/physical-models`、`/api/router/capabilities`、`/_proxy/health`（`slot_management` 字段应与本机实例一致：auto_heal=true / idle 900s）。

## 后续升级流程

工作站 `cargo build --release` → `scp` 新二进制到 `/opt/llm-provider-router/llm-provider-router` → 容器内走 `stage`（重启非活跃槽加载新二进制）→ probe → `switch`，与 skill 的两段式流程相同。注意 stage 前确认容器侧部署锁空闲（`lock-status`）。

## DDNS（llmrouter.hevinlacia.top）

AAAA 记录 → 容器 GUA（电信动态前缀），updater 在容器内自更新（2026-10-17 上线）：

| 项 | 值 |
|---|---|
| 脚本 | `/opt/ddns-aliyun/ddns-aliyun.py`（Python 标准库实现阿里云 RPC 签名，零依赖；选 eth0 稳定 SLAAC GUA，跳过 temporary/deprecated） |
| 单元 | `ddns-aliyun.service`（oneshot）+ `ddns-aliyun.timer`（系统级，OnBootSec=90s，之后每 2min；值不变不调 API） |
| AK | `/root/.config/ddns-aliyun/ddns.env`（600，RAM 子账号最小策略 alidns Describe/Add/Update DomainRecord） |
| 手动跑 | `systemctl start ddns-aliyun.service && journalctl -u ddns-aliyun -n 3` |
| 行为 | 无记录→Add；值相同→no-op；值不同→Update；exit 2=env 缺失，exit 3=API 错 |

注意：脚本源码 git 管理于 `~/Developer/infra/ddns-aliyun/`（README 含重新部署步骤），容器内为部署目标 `/opt/ddns-aliyun/`；fake-ip 劫持（198.18.x）下本机明文 DNS 看不到真实解析值，验证用 DoH `https://223.5.5.5/resolve?name=llmrouter.hevinlacia.top&type=AAAA`。

## 踩坑与注意

- **bootstrap 后 status 报「旧 proxy,需升级」**：启动竞态误报，等几秒重查 `/_proxy/health` 的 `slot_management` 即可，不代表二进制旧。
- **非交互 ssh 操作 user 单元**必须带 `XDG_RUNTIME_DIR=/run/user/0`，否则 `systemctl --user` 报连不上 bus。
- **root linger 是前提**：没 enable-linger 的话 SSH 断开 user manager 即停。
- **agent-env.conf 是整文件拷贝**，含工作站全部 agent secrets（不止 router 用的），600/root；若容器定位变为共享/一次性，应裁剪为仅 `AGENT_AI_*` 相关变量。
- 容器内没有 cargo/node，`install-service.sh` 的构建段落不适用，不要在容器里跑该脚本。
