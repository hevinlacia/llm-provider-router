# 客户端访问端点动态解析（三通道地址）

> 用于：每台机器访问 router 的地址是机器属性（内网 / Tailscale / 公网），本机制让地址可动态
> 更新而不必改客户端代码。触发词：router 地址、端点候选表、router_url、地址动态更新、换网。

## 机制（与 Agent Panel 的 `/api/panel/endpoints` + `panel_url.py` 同构）

```
router（SSOT 候选表）        GET/PUT /api/router/endpoints → <state_dir>/router-endpoints.json
    │
    ├── 每台机器：router_url.py 探活选路 → ~/.cache/llm-router/router-url.json（TTL 300s）
    │        └── router-url-probe.timer（5min --refresh，systemd user）
    │
    └── 消费者：pi extension router-context-sync（读缓存）、systemd EnvironmentFile（router.env）、脚本（import resolve_router_url）
```

解析顺序（`~/Developer/.agents/scripts/router_url.py`）：

1. `LLM_PROVIDER_ROUTER_BASE_URL` 环境变量（不探测）
2. `~/.config/llm-router/endpoint.toml` 的 `url =` 钉死（不探测）
3. 缓存 `~/.cache/llm-router/router-url.json`（TTL 内直接用）
4. 探活 bootstrap → 拉权威候选表 → 按本机 `priority` / 表序探活
5. 全不可达 → 过期缓存（degraded）→ 内置默认（`--strict` 时 exit 1）

## 候选表（内置默认）

| name | url | 说明 |
|---|---|---|
| `lan` | `http://192.168.6.245:8789` | 内网直连，同 LAN 时最快 |
| `tailscale` | `https://edge.tail260d93.ts.net/llm-router` | Caddy `strip_prefix /llm-router` → 内网后端（带子路径前缀） |
| `public` | 空（停用占位） | 公网兜底；启用前需先修 `llmrouter.hevinlacia.top` 证书（caddy-cert-renew 失败中） |

`url` 空 = 该候选停用。候选 url 即 **base**（可含子路径），客户端自行拼 `/v1`、`/api/router/*`、`/health`。

## 运维

```bash
# 查看/修改候选表（数组顺序 = 全局默认优先级）
curl -s http://192.168.6.245:8789/api/router/endpoints | jq .endpoints
curl -s -X PUT http://192.168.6.245:8789/api/router/endpoints \
  -H 'Content-Type: application/json' \
  -d '{"endpoints":[{"name":"lan","url":"http://192.168.6.245:8789","note":"内网直连"},{"name":"tailscale","url":"https://edge.tail260d93.ts.net/llm-router","note":"Tailscale"}]}'

# 本机解析（换网后立即生效）
python3 ~/.agents/scripts/router_url.py --refresh --json

# 本机覆盖优先级（如本机应优先走 tailscale）
mkdir -p ~/.config/llm-router && printf 'priority = ["tailscale","lan"]\n' > ~/.config/llm-router/endpoint.toml
```

## 部署位置

router 真身：LXC `llm-router`（`192.168.6.245`），`/opt/llm-provider-router`，user 级 systemd
（`llm-provider-router.service` + `llm-provider-router-backend@{blue,green}.service`）。
更新流程：本机 `cargo build --release` → scp 二进制到 `/opt/llm-provider-router/llm-provider-router.new`
→ 远端 `mv` + `python3 bin/hot-deploy-router.py deploy`（blue/green，含 precheck 与自动回滚）。
