# hot-deploy 二进制源漂移导致生产功能回退

用于：llm-provider-router 走 `bin/hot-deploy-router.py deploy` 部署前，确认构建源包含本地 main 的全部提交，避免"部署了旧代码、回退新功能"的事故。
触发词：deploy 回退、功能回退、worktree 构建、origin/main 漂移、本地 main 领先、二进制不一致、chat 协议又回来了。
不适用：blue/green 槽操作细节与复检清单（→ `llm-router-deploy` skill）；日常分支开发流程（→ `tools/AGENTS.md`）。

## 事故模式（2026-10 实际发生）

worktree 按 tools 规范基于 **origin/main** 创建（`git worktree add ... -b feat/x origin/main`），而本地 main 领先 origin/main 数个提交（如 responses 协议迁移、`/v1/chat/completions` 下线、报错模板化）。在 worktree 里 `cargo build --release` 后把二进制放到主仓 `target/release/` 再 deploy——**生产被回退到 origin/main 的功能集**：已下线的 chat 协议复活、新协议迁移丢失。

根因：systemd backend unit 的 ExecStart 硬编码主仓 `target/release/llm-provider-router`，deploy 拉起的就是这个文件；脚本不校验"二进制构建源 == 本地 main HEAD"。

## 部署前强制检查

```bash
cd <repo>
git fetch origin
git log --oneline origin/main..main   # 本地领先提交数；非 0 时必须基于 main HEAD 构建
```

在**主仓 main HEAD**（不是 worktree）构建 release，或 worktree 构建前先变基到本地 main。合并 worktree 分支回 main 后，若合并带来新代码（如冲突解决后的移植），**必须再基于 main HEAD 重建并重跑一次 deploy**——合并后的源码 ≠ 分支上的构建产物。

## 症状识别

部署后复检发现"已下线的端点复活"（如 `/v1/chat/completions` 不再 405）、新路由/字段消失（如 `/api/config/search-providers` 缺新键）、`bindings` 数变少——都指向"二进制比 main 旧"。

## 相关事实

- `target/release` 覆盖用 `mv`（换 inode）不用 `cp`（写同 inode，影响运行中进程）；运行中的 blue 槽不受文件替换影响。
- 合并 worktree 分支时若 main 重组过目录（如 handler 从 `routes/chat.rs` 搬到 `routes/search.rs`），冲突解决后要在主仓重跑 `cargo test` 全量验证。
