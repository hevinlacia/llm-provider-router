---
name: llm-router-deploy
description: llm-provider-router 的 blue/green 两段式部署流程：stage（只部署非活跃槽）→ test（直连槽端口做协议级探针）→ switch（测试全过后切流），含客户端协议审计与秒级回滚。触发词：部署 router、发布新版本、hot-deploy、切流、切槽、部署后 405、部署后不可用、blue/green、回滚 router、无感切换。
allowed-tools: ["bash", "read"]
---

# LLM Router 部署（stage → test → switch）

用于：部署 llm-provider-router 新版本，保证切流前后客户端全程无感、出问题秒级回滚。

适用：发布新版本、重新部署某个槽、部署后出现 405/不可用的排查与回滚、新协议端点上线前的验证。

不适用：front-proxy 自身升级（先于 backend 走自己的升级，见项目 AGENTS.md）、运行时配置热加载（watcher 自动生效，无需部署）。

## 铁律（2026-10-08 事故教训，违反即可能造成客户端大面积 405/不可用）

1. **删除/变更客户端协议端点 = 破坏性变更**：先跑「阶段 0 客户端审计」，确认无客户端使用才允许删。`/health` ok 不代表协议可用——它只证明进程活着。
2. **部署全程必须持锁（2026-10-08 事故：两会话同时部署互相覆盖槽位状态）**：整个 stage→probe→switch 流程必须持有部署互斥锁（`flock` 文件锁 + token）；拿不到锁 = 有其他部署在进行 → 等待或放弃，**绝不并行部署**。front-proxy 对持锁期间的切流请求校验 `X-Deploy-Lock: <token>`，不匹配返回 409。
3. **两段式强制**：新版本必须先 stage 到非活跃槽、探针全过，才允许 switch 切流。禁止部署完立即切流。
4. **探针必须直连槽端口**（绕过 front-proxy），用 POST 打客户端真实使用的每个协议端点——GET 打管理 API 探不出协议破坏。
5. **回滚路径全程可用**：回滚 = `switch --slot {previous_slot} --lock-token <T>`（或持锁前的紧急路径：先 `lock-release --force` 再切）；旧槽保持运行，禁止手动停。

## 阶段 0：客户端协议审计（每次必做）

扫描本机所有指向 router(:8789) 的客户端及其 api 协议：

```bash
# pi（注意：provider 级 api 与模型级 api 覆盖可能不同，两层都要看）
python3 -c "import json;d=json.load(open('$HOME/.pi/agent/models.json'));[print(pid,p.get('api'),[(k,(m or {}).get('api')) for k,m in ((p.get('models') or {}).items() if isinstance(p.get('models'),dict) else [])]) for pid,p in d.get('providers',{}).items() if '8789' in str(p.get('baseUrl',''))]"
# 其他本机服务
grep -rn "127.0.0.1:8789\|localhost:8789" ~/.config/opencode ~/.hermes ~/.pi --include="*.json" --include="*.toml" --include="*.yaml" -l 2>/dev/null
```

结论写进部署记录：**哪些端点有真实客户端** → 这些端点在阶段 2 是硬性 PASS 项（405/缺失 = FAIL）。若要删除某端点，必须先完成客户端迁移并复查本步骤。

## 阶段 1：stage（只部署到非活跃槽，不动流量）

```bash
cd <repo-root> && python3 bin/hot-deploy-router.py lock-status   # 确认当前无人持锁
cargo build --release                                            # 主检出区构建
python3 bin/hot-deploy-router.py stage                           # 默认目标=非活跃槽；重启目标槽拉起新二进制，并获取+持有部署锁
```

`stage` 成功输出 `lock_token=<T>`（fork 出的 holder 子进程持锁，跨调用存活；被占用时默认等 300s，`--lock-wait 0` 可 fail fast）。**后续所有步骤都要带这个 token**；锁状态随时可查：`python3 bin/hot-deploy-router.py lock-status` 或 `curl :8789/_proxy/deploy-lock`。

此阶段 front-proxy 流量仍走旧槽，客户端零感知。

## 阶段 2：test（直连非活跃槽端口，协议级探针）

```bash
python3 <本skill目录>/scripts/probe_slot.py --port {inactive-port} --model {常用逻辑模型}
```

脚本逐项探测并输出 PASS/FAIL（探针矩阵见 `references/probes.md`）：

- `POST /v1/responses` → 200
- `POST /v1/chat/completions` → 200；**405 = FAIL**（除非阶段 0 审计已批准删除，加 `--allow-chat-removal` 降级为 WARN）
- `POST /v1/messages` → 405 = FAIL（400/404 = 端点存在，PASS）
- `GET /api/state、/api/errors/recent、/api/config/error-rules、/api/router/capabilities` → 200 且 JSON（返回 HTML dashboard = FAIL）
- 真实模型冒烟（加 `--smoke`，产生少量上游调用）：小请求走通上游 key 链路

任一 FAIL → 禁止进入阶段 3；修复代码重走阶段 1-2。

## 阶段 3：switch（切流）+ 复检

```bash
python3 bin/hot-deploy-router.py switch --slot {staged_slot} --lock-token {T}   # 校验锁→切流→front-proxy 复检，FAIL 自动回滚
python3 <本skill目录>/scripts/probe_slot.py --port 8789 --model {model}          # 可选：再跑一轮完整探针
python3 bin/hot-deploy-router.py lock-release --token {T}                        # 确认无问题后释放锁
```

- 旧槽保持运行（勿停），供秒级回滚；15min 无流量被 front-proxy 自动下线属正常。
- 若用旧版 hot-deploy 脚本的 `deploy`（部署+切流一体，内部自动持锁）：仅当本次变更**不涉及端点/协议/路由表**时才允许，且切完立即跑阶段 2 探针，FAIL 即回滚。

## 回滚（任何异常，秒级）

```bash
# 正常路径：仍在锁持有期内，直接带 token 切回
python3 bin/hot-deploy-router.py switch --slot {previous_slot} --lock-token {T}
# 紧急路径：锁持有者失联/僵死 → 强制释放后切回
curl -X POST http://127.0.0.1:8789/_proxy/active/{previous_slot}   # 锁空闲时 proxy 直接放行
python3 bin/hot-deploy-router.py lock-release --token {任意} --force
```

回滚后新槽标记"待修复"，修复后从阶段 1 重走。客户端侧症状（405/连接错误）先对照阶段 0 的协议清单定位，不要先怀疑 key。

## 部署后观察（前 10 分钟）

- `curl :8789/api/errors/recent`：看新增错误的分类/key/原文（报错分类功能上线后自带）
- `journalctl --user -u llm-provider-router-backend@{slot} --since '-10 min'`
- 客户端侧报错：405/404 → 协议端点匹配问题；429/400 → key/上游问题（走报错分类排查）

## 事故档案

完整证据链与教训：read `references/incident-2026-10-08-405.md`（删 chat 端点导致 pi low-model-auto 及多个服务 405 约 50 分钟；`/_proxy/active/blue` 秒级回滚恢复）。

## Final Response

部署报告必须包含：阶段 0 审计结论（客户端 × 协议清单）、stage 槽位与构建版本、探针逐项结果、switch 时刻、切流后复检结果、回滚命令（原样给出）。
