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

## 候选表（内置默认，顺序即优先级）

| 顺序 | name | url | 说明 |
|---|---|---|---|
| 1 | `lan` | `http://edge.lan/llm-router` | 内网域名 80 端口路径式（immortalwrt `.lan` → edge 容器）。真 LAN 直连，**不走 subnet** |
| 2 | `tailscale` | `https://edge.tail260d93.ts.net/llm-router` | Tailscale 域名（HTTPS，Caddy `strip_prefix` 子路径） |
| 3 | `public` | 空（停用占位） | 公网兜底；需先修 `.245` 上 `caddy-cert-renew`（`llmrouter.hevinlacia.top` 证书） |
| 4 | `subnet` | `http://192.168.6.249/llm-router` | **最低优先级**：经 Tailscale subnet 中转访问 edge 内网 IP（80 路径式） |

`url` 空 = 该候选停用。候选 url 即 **base**（可含子路径），客户端自行拼 `/v1`、`/api/router/*`、`/health`。

### 两条硬约束

1. **不写服务容器地址**（`192.168.6.245:8789`）：客户端一律走 edge 容器入口。直连服务容器会绕过
   入口层（子路径前缀、压缩、守卫、SSE flush 策略都在 edge）。`subnet` 候选同样指向 edge 的内网 IP。
2. **LAN/subnet 都走 80 端口路径式**（`/llm-router` 前缀，与 tailscale 入口同风格）：edge caddy 新增
   内网站点 `bind 192.168.6.249`（`http://edge.lan`、`http://192.168.6.249`），不再暴露自定义端口 8789。
2. **LAN 用域名不用 IP**：内网由 immortalwrt 提供 `.lan` 域名。从 tailnet 机器访问 `192.168.6.x`
   实际经 immortalwrt 广播的 `192.168.6.0/24` subnet 路由中转，**不是真 LAN**，必须与内网域名分开
   成两条候选，且 subnet 排最后。

### 已知限制（2026-10-28 实测）

`subnet` 通道目前对 edge **不可达**：edge 的 tailscale 防火墙规则
`ts-input: -s 100.64.0.0/10 ! -i tailscale0 -j DROP` 会丢弃「从 eth0 进来但源是 tailnet 地址」的
subnet 转发包（`nc 192.168.6.249:22`/`:8789` 均超时；而 `192.168.6.245:8789` 可达）。因此该候选
在当前边缘配置下探活失败即跳过，属预期降级。要让这条通道真正可用，需在 edge 放行
`iifname eth0 saddr 100.64.0.0/10 tcp dport 8789 accept`（安全相关，需用户决策）。

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
更新流程：本机 `cargo build --release` → **配置漂移检查 `python3 bin/config-drift-check.py --host root@192.168.6.245`
（2026-10-29 zp 限额事故：error-rules.json 未随二进制上机导致限额不切 key；退出码 1 = 有漂移，
`--diff` 确认方向后 `--sync` 推送，dashboard 改过的配置以远端为准；装了 just 的机器可用 `just config-drift`）**
→ scp 二进制到 `/opt/llm-provider-router/llm-provider-router.new`
→ 远端 `mv` + `python3 bin/hot-deploy-router.py deploy`（blue/green，含 precheck 与自动回滚）。
