# claude-pty-controller

把一台运行 [Claude Code](https://claude.com/claude-code) 的机器，通过**单一静态 Rust 二进制**暴露为可远程观测 / 驱动的终端会话。`scp` 一个文件即部署，无运行时依赖。

claude 跑在 **tmux** 会话里：控制器 / SSH 断开不影响它继续运行，本地终端可随时 `tmux attach` 与远程 Dashboard **双向透明**共享同一个 claude。

**三端 + 端到端加密**：被控端（本仓库）/ 中转端 Relay / 控制端 Dashboard 三端模型，两端主动外连 relay（NAT 友好）。中间流量对 relay **零知识** —— wss 逐跳 TLS + Noise 端到端加密，relay 只转发不透明密文、读不到内容。详见 ARCHITECTURE.md §13–§14。

## 三条数据通道

| 通道 | 数据源 | WebSocket 消息 |
|------|--------|----------------|
| 终端画面 | PTY stdout | `{"type":"output","raw":"…"}` |
| 对话内容 | JSONL 文件 | `{"type":"transcript","message":{…}}` |
| 状态事件 | OSC 序列 | `{"type":"event","event":"tab_status","status":"Working…"}` |

入站（远程 → PTY）：`input` / `raw` / `resize` / `refresh`。通道二除轮询外，还由状态事件（回合结束）和手动 `refresh` 触发刷新。

## 文档

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 落地架构设计（§10 含相对初版方案的关键修正与原因）

## 状态

设计阶段。实现里程碑见 ARCHITECTURE.md §11。

## 安全

控制器具备远程 shell 注入能力并访问宿主机 `ANTHROPIC_API_KEY`。**必须**设置 `CONTROL_TOKEN` 并使用 `wss://`，详见 ARCHITECTURE.md §5。

## 构建

```bash
cargo build --release   # → target/release/claude-pty-controller
```
