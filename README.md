# claude-pty-controller

把一台运行 [Claude Code](https://claude.com/claude-code) 的机器，通过**单一静态 Rust 二进制**暴露为可远程观测 / 驱动的终端会话。`scp` 一个文件即部署，无运行时依赖。

## 三条数据通道

| 通道 | 数据源 | WebSocket 消息 |
|------|--------|----------------|
| 终端画面 | PTY stdout | `{"type":"output","raw":"…"}` |
| 对话内容 | JSONL 文件 | `{"type":"transcript","message":{…}}` |
| 状态事件 | OSC 序列 | `{"type":"event","event":"tab_status","status":"generating"}` |

入站（远程 → PTY）：`input` / `raw` / `resize`。

## 文档

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 落地架构设计
- [docs/REVIEW.md](docs/REVIEW.md) — 对初版方案的技术评审
- [方案6-Rust版.md](方案6-Rust版.md) — 原始方案稿

## 状态

设计阶段。实现里程碑见 ARCHITECTURE.md §11。

## 安全

控制器具备远程 shell 注入能力并访问宿主机 `ANTHROPIC_API_KEY`。**必须**设置 `CONTROL_TOKEN` 并使用 `wss://`，详见 ARCHITECTURE.md §5。

## 构建

```bash
cargo build --release   # → target/release/claude-pty-controller
```
