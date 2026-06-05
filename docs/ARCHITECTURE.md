# claude-pty-controller — 架构设计

> 把一台运行 Claude Code 的机器，通过单一静态 Rust 二进制暴露为可远程观测 / 驱动的终端会话。
> 本文是在 `方案6-Rust版.md` 基础上、修正 `docs/REVIEW.md` 所列缺陷后的落地设计。

## 1. 目标与非目标

**目标**
- 单一静态二进制（`scp` 即部署，无运行时依赖）。
- 三条数据通道，与现有 Dashboard 的 WebSocket JSON 协议保持兼容（语义不变，个别字段按本文修正）。
- 远程可观测（终端画面 + 结构化对话 + 状态事件）且可驱动（输入注入 / 控制字符 / resize）。
- 默认安全：强制鉴权，支持 `wss://`（rustls，无 OpenSSL）。

**非目标（v1）**
- 多会话 / 多路复用（v1 单会话）。
- 录制回放、持久化历史（仅内存快照）。
- Windows（`portable-pty` 支持，但 v1 只验证 Unix）。

## 2. 进程模型

采用 **Tokio 异步运行时**，而非稿件的手写多线程 —— 这是修正 REVIEW B1（WebSocket 帧拆分）最干净的方式：`tokio-tungstenite` 的 `.split()` 给出各自持有正确帧状态的 `SplitSink`/`SplitStream`，天然支持读写并发、背压、重连。

```
main (tokio runtime)
│
├─ task: pty_reader      PTY master → (通道一 output + 通道三 OSC event) → out_tx
├─ task: jsonl_watcher   tail JSONL  → (通道二 transcript)               → out_tx
├─ task: ws_outbound     out_rx → ws_sink.send()        （唯一持有 sink 的任务）
├─ task: ws_inbound      ws_stream.next() → 解析 Incoming → pty_in_tx
└─ task: pty_writer      pty_in_rx → master.write()      （唯一持有 writer 的任务）
                                       │
                            Resize    └→ master.resize()（持有 master 句柄）
```

- `out_tx/out_rx`：**有界** `mpsc`（如容量 1024），背压策略见 §7。所有出站消息（三通道）汇入这里，由单一 `ws_outbound` 任务串行写 sink —— 保证帧不交错。
- `pty_in_tx/pty_in_rx`：入站命令队列，由单一 `pty_writer` 任务消费。
- `portable-pty` 的 I/O 是阻塞的：PTY 读、写、JSONL 读都跑在 `tokio::task::spawn_blocking` 或专用线程里，通过 channel 与 async 世界通信。

### 关于 master 句柄（修正 B4）
`PtySession` 必须**保留 `master`**：`writer`（写子进程 stdin）、`reader`（读子进程 stdout，`try_clone_reader`）、`master`（用于 `resize()`）、`child`（监测退出）。resize 通过 `master.resize(PtySize{rows,cols,..})` 实现，**绝不**往 PTY 写 `\x1b[8;..t`。

## 3. 三条数据通道

| 通道 | 数据源 | 出站消息 |
|------|--------|----------|
| 一 · 终端画面 | PTY stdout | `{"type":"output","raw":"…"}`（见 §3.1 编码） |
| 二 · 对话内容 | JSONL 文件 | `{"type":"transcript","message":{…}}` |
| 三 · 状态事件 | OSC 序列 | `{"type":"event","event":"…", …}`（见 §3.3 schema） |

### 3.1 通道一 · 终端画面（修正 B3）

PTY 输出是二进制，UTF-8 多字节会被切在读边界。**不使用 `from_utf8_lossy`。**

**方案 A（默认，保持线缆兼容）— 尾字节缓冲**
维护 `pending: Vec<u8>`。每次读到 `buf`，拼到 `pending`，用 `str::from_utf8` 找最长合法前缀，发其 UTF-8 字符串；把不完整的尾字节留在 `pending`。ANSI 控制字节全 ASCII，绝不被截，安全无损。`raw` 仍是合法 UTF-8 字符串，Dashboard 零改动。

**方案 B（可选，最稳）— base64**
`{"type":"output","enc":"base64","data":"…"}`，Dashboard 端 `term.write(atob(data))`。彻底规避编码问题，但需前端加一行解码。

> v1 用方案 A（兼容优先）；若发现异常字节流再切 B。两者可由 CLI flag 切换。

### 3.2 通道二 · 对话内容（修正 M1）

JSONL 路径：`~/.claude/projects/<project-path>/<session-id>.jsonl`。每行一条结构化消息（`user` / `assistant`，含 `tool_use.input`、`tool_result.content`），原样转发为 `transcript`。

**增量 tail 的正确做法**：游标按「**最后一个换行符之后**」推进，而非文件长度。

```
last_offset = 0
loop (poll 250ms 或 notify 事件):
    len = metadata(path).len()
    if len < last_offset: 文件被截断/换 session → 重新定位，last_offset = 0
    if len > last_offset:
        seek(last_offset); read 到 EOF 进 buf
        切出 buf 中最后一个 '\n' 之前的完整部分，逐行 serde_json 解析、转发
        last_offset += （最后一个换行符的位置 + 1）   // 未闭合尾行留到下次
```

- session 定位：优先按当前 session-id 锁定；找不到时回退「mtime 最新且非 `timeline.jsonl`」。文件消失/截断时重新发现（已知限制：多 session 并发时定位会抖动，文档标注）。
- 文件监视：v1 用 250ms 轮询（实现简单、跨平台）；可选 `notify`（inotify）降延迟。

### 3.3 通道三 · 状态事件（修正 B5 / M3 / M4）

**OSC 协议**（从 Claude Code 提取）：

- Tab 状态 `OSC 21337`：`ESC ] 21337 ; indicator=#rrggbb ; status=generating ; status-color=#rrggbb BEL`，关心 `status`（`generating`/`waiting`/`approval`/`idle`）与 `indicator` 颜色。
- 进度条 `OSC 9 ; 4`（iTerm2）：`ESC ] 9 ; 4 ; <op> ; <pct> BEL`，op：0 清除 / 1 设置 / 2 错误 / 3 不确定。
- 标题 `OSC 0` / `OSC 2`：`ESC ] 0 ; <title> BEL`。
- 通知铃 `BEL (0x07)`：任务完成 / 权限请求。
- tmux 包装：`ESC P tmux ; <escaped> ESC \`，内部 `ESC` 翻倍。

**状态机实现要点（修正 M3）**：整机在 **`&[u8]`** 上运行，缓冲用 `Vec<u8>`，字段提取时才 `str::from_utf8`。状态：`Ground` / `Esc`（见过 ESC）/ `Osc`（ESC ]，累积到 `BEL` 或 `ST=ESC\`）/ `Dcs`（ESC P，累积到 `ESC\`）。

- OSC 终止后，剥掉前缀 `ESC ]` 与终止符（BEL 或 ST），按首个 `;` 切出命令号，分派 0/2、9、21337。
- DCS（tmux，修正 M4）终止后：剥前缀 `\x1bPtmux;` 和后缀 `\x1b\\`，把内部 `\x1b\x1b` 还原为 `\x1b`，再把还原出的字节喂回 OSC 解析。
- 字节按 8KB 块喂入，状态跨块保留，避免任何按字节切片 panic。

**出站 Event 消息 schema（修正 B5）**：定义单一结构体，扁平 `Option` 字段：

```rust
#[derive(Serialize)]
struct EventMsg {
    r#type: &'static str,        // 恒为 "event"
    event: &'static str,         // "tab_status" | "progress" | "title" | "bell"
    #[serde(skip_serializing_if = "Option::is_none")] status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] indicator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] percentage: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] title: Option<String>,
}
```

线缆示例：`{"type":"event","event":"tab_status","status":"generating","indicator":"#00cc00"}`。

## 4. 入站协议（远程 → PTY）

```json
{"type":"input","text":"帮我重构这个模块"}     // 追加 \r 后写入
{"type":"raw","text":""}                  // Ctrl+C； Ctrl+D；[A ↑
{"type":"resize","cols":200,"rows":50}          // → master.resize()
```

`input` 在文本后补 `\r`；`raw` 原样写入控制字符；`resize` 调 `master.resize()`（**非**写转义序列）。所有入站经 `pty_writer` 单任务串行执行。

## 5. 安全（修正 M2）

**威胁模型**：控制器 = 远程 shell 注入能力 + 宿主机 `ANTHROPIC_API_KEY` 访问。任何能连上 WS 端点的人即可驱动该机器。因此鉴权是必需项，不是可选项。

- **鉴权**：连接时 `Authorization: Bearer <token>` 头或首帧 `{"type":"auth","token":"…"}`；token 来自环境变量 `CONTROL_TOKEN`，未设置则**拒绝启动**。校验失败立即关闭连接。
- **传输**：生产强制 `wss://`（rustls）。`ws://` 仅允许 `127.0.0.1` / 显式 `--insecure`。
- **最小权限**：以非 root 运行；env 注入跳过空值（修正 m2）；不打印 token/key。
- **可选**：入站命令审计日志、来源 IP 允许列表。

## 6. 依赖（修正 B2）

```toml
[dependencies]
portable-pty = "0.8"
tokio = { version = "1", features = ["rt-multi-thread","macros","sync","time","io-util"] }
tokio-tungstenite = { version = "0.24", default-features = false, features = ["connect","rustls-tls-webpki-roots"] }
futures-util = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
dirs = "5"
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
# 可选：notify = "6"（JSONL inotify）、vt100 = "0.15"（重连屏幕快照）、base64 = "0.22"（通道一方案 B）

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

**全链纯 Rust，TLS 走 rustls + webpki-roots，无 OpenSSL / 无 C/C++。** 二进制体积比纯 tungstenite 版略大（tokio + rustls），仍是单文件、数 MB 级、`scp` 即部署。

## 7. 背压与重连（修正 M5）

- `out_tx` 用**有界** channel（容量 1024）。WS 已连：正常发送。WS 断开：`ws_outbound` 不消费，channel 满后 `try_send` 失败 → 对**通道一 output** 采用「丢旧」策略（终端画面是可重建的最终态），对**通道二/三**尽量保序不丢（容量内）。
- **重连快照**：v1 标注限制——新接入只见后续输出。v2 引入 `vt100` 维护屏幕缓冲，新连接先发一帧 `{"type":"snapshot","screen":"…"}`，再转入实时流。
- WS 重连：指数退避（2s 起，封顶），重连后重新鉴权。

## 8. 生命周期

- 启动：解析 env/flag → 校验 `CONTROL_TOKEN` → 起 PTY（直接 spawn `claude`，可配置回退到 `$SHELL`，修正 m3）→ 起各 task → 连 WS。
- 关闭：`SIGINT`/`SIGTERM` 触发 `CancellationToken`；各 task 收尾；向子进程发 `Ctrl+D`；等待 child 退出（修正 m4）。
- 子进程退出：`pty_reader` 读到 EOF → 通知主流程优雅关闭。

## 9. 部署

```bash
cargo build --release          # → target/release/claude-pty-controller
scp target/release/claude-pty-controller user@server:/opt/

CONTROL_TOKEN=$(openssl rand -hex 32) \
REMOTE_URL=wss://your-relay:9000 \
ANTHROPIC_BASE_URL=https://your-proxy.com \
ANTHROPIC_API_KEY=sk-xxx \
RUST_LOG=info \
/opt/claude-pty-controller
```

## 10. 与稿件的差异小结

| 项 | 稿件 | 本设计 | 原因 |
|----|------|--------|------|
| 并发模型 | 手写多线程 + 裸流克隆 | tokio + `.split()` | B1 帧封装 |
| TLS | 无 feature，仅 ws | rustls-tls-webpki-roots | B2 |
| 通道一编码 | `from_utf8_lossy` | 尾字节缓冲 / base64 | B3 |
| resize | 写 `\x1b[8;..t` | `master.resize()` | B4 |
| Event 类型 | 枚举 + `..` | 扁平 `Option` 结构体 | B5 |
| JSONL 游标 | 文件长度 | 末换行偏移 | M1 |
| 鉴权 | 无 | 强制 token + wss | M2 |
| OSC 状态机 | `byte as char` String | `&[u8]` | M3/M4 |
| 背压 | 无界 channel | 有界 + 丢旧 | M5 |

## 11. 里程碑

1. **M1 骨架**：PTY spawn + 通道一（尾字节缓冲）+ ws_outbound，本地 `ws://127.0.0.1` 跑通画面。
2. **M2 双向**：入站 input/raw/resize（`master.resize`）+ 鉴权 + wss。
3. **M3 通道二/三**：JSONL tail（末换行游标）+ OSC 状态机（`&[u8]` + tmux）。
4. **M4 健壮性**：有界背压 + 重连退避 + 优雅关闭 + 子进程退出处理。
5. **M5（可选）**：vt100 重连快照、notify、base64 模式、审计日志。
