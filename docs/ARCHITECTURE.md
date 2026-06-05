# claude-pty-controller — 架构设计

> 把一台运行 Claude Code 的机器，通过单一静态 Rust 二进制暴露为可远程观测 / 驱动的终端会话。
> 本文是在初版方案 `方案6-Rust版.md`（源文件已删除,保留在 git 历史中）基础上、修正其若干缺陷后的落地设计；与原方案的关键差异及原因汇总见 §10。

## 1. 目标与非目标

**目标**
- 单一静态二进制（`scp` 即部署，无运行时依赖）。
- 三条数据通道，与现有 Dashboard 的 WebSocket JSON 协议保持兼容（语义不变，个别字段按本文修正）。
- 远程可观测（终端画面 + 结构化对话 + 状态事件）且可驱动（输入注入 / 控制字符 / resize）。
- **会话后台留存**：claude 跑在 **tmux** 会话里，控制器 / SSH 断开不影响 claude 继续运行，控制器重启后无缝接管同一会话。
- **本地双向透明**：本地终端可随时 `tmux attach` 接入同一会话，与远程 Dashboard **同时、透明**地观看并驱动同一个 claude（tmux 原生多客户端镜像）。
- **对话内容低延迟**：通道二除轮询外，由**状态事件触发**和**用户手动刷新**两条额外路径驱动重读（见 §3.2）。
- 默认安全：强制鉴权，支持 `wss://`（rustls，无 OpenSSL）。

**非目标（v1）**
- 多会话 / 多路复用（v1 单会话）。
- 录制回放、持久化历史（仅内存快照）。
- Windows（`portable-pty` 支持，但 tmux 模型为 Unix-only，v1 只验证 Linux/macOS）。

## 2. 进程模型

采用 **Tokio 异步运行时**，而非原方案的手写多线程 —— 这是规避「裸流克隆绕过 WebSocket 帧封装/掩码」问题最干净的方式：`tokio-tungstenite` 的 `.split()` 给出各自持有正确帧状态的 `SplitSink`/`SplitStream`，天然支持读写并发、背压、重连。

```
本地终端 ──(tmux attach -t claude-ctl, 可选, 双向透明)──┐
                                                       ▼
            ┌─ tmux server（后台留存, 进程独立于控制器）──────────┐
            │   session "claude-ctl"  ·  pane 运行 `claude`         │
            └───────────────────────────────────────────────────────┘
                         ▲  控制器作为另一个 tmux client（PTY）  ▼
main (tokio runtime)
│
├ task: pty_reader   tmux-client PTY → 通道一 output ─────────────► out_tx
│                                    → 通道三 OSC 事件 ───────────► out_tx
│                                            │
│                         tab_status: Working… → Idle/Waiting（回合结束）/ bell
│                                            └──────────────────► refresh_tx
│
├ task: jsonl_watcher   轮询 250ms  ∪  refresh_tx → tail JSONL → 通道二 transcript → out_tx
│
├ task: ws_outbound  out_rx → ws_sink.send()          （唯一持有 sink）
├ task: ws_inbound   ws_stream.next() → Incoming → pty_in_tx
│                                  └ {"type":"refresh"} ─────────► refresh_tx（手动刷新）
└ task: pty_writer   pty_in_rx → master.write() / master.resize()
```

- `out_tx/out_rx`：**有界** `mpsc`（如容量 1024），背压策略见 §7。所有出站消息（三通道）汇入这里，由单一 `ws_outbound` 任务串行写 sink —— 保证帧不交错。
- `refresh_tx`：通道二的**三个刷新触发源**汇流口——轮询定时器、通道三状态事件、远程手动 `refresh`（见 §3.2）。
- `pty_in_tx/pty_in_rx`：入站命令队列，由单一 `pty_writer` 任务消费。
- `portable-pty` 的 I/O 是阻塞的：PTY 读、写、JSONL 读都跑在 `tokio::task::spawn_blocking` 或专用线程里，通过 channel 与 async 世界通信。

### 2.1 tmux 持久化与双向透明

控制器**不直接 spawn `claude`**，而是在自己的 PTY 里跑一个 tmux 客户端，让 claude 活在一个**可重连的 tmux 会话**中：

```
tmux -L claude-ctl new-session -A -s claude-ctl -x 160 -y 45 'claude'
```

- `-A`：会话存在则 attach、不存在则新建 —— 控制器**崩溃 / SSH 断开后重启，用同一条命令无缝接管**，claude 期间一直在 tmux server 里跑（后台留存）。
- `-L claude-ctl`：独立的 tmux socket，避免和用户既有 tmux 串台。
- **双向透明**：tmux 原生支持多客户端镜像。本地用户 `tmux -L claude-ctl attach -t claude-ctl` 即可接入**同一个 pane**，与控制器的 client、远程 Dashboard 同时看到同一屏、同时可输入 —— 三方透明共享一个 claude 会话。

**OSC 直通（关键）**：claude 检测到 `$TMUX` 后会把状态 OSC 用 DCS 包裹（`\x1bPtmux;…\x1b\\`，内部 ESC 翻倍）。tmux **默认丢弃** 该 DCS，必须显式开启直通，控制器在建会话后执行：

```
tmux -L claude-ctl set -g allow-passthrough on   # 否则收不到 OSC 21337/9;4/标题
tmux -L claude-ctl set -g window-size latest      # 多客户端时以最新 attach 的尺寸为准，避免被缩到最小
```

开启后 tmux 会**解包**内层序列再转发给每个 client 的终端 —— 即控制器 PTY 读到的是**已解包的裸 OSC**。但状态机仍需保留 DCS 解包路径，以兼容非 tmux / GNU screen（`$STY`）场景（§3.3）。
- **铃声 caveat**：claude 发的是**裸 BEL**（未包裹）；tmux 会把它当 pane 的 bell 事件按 `bell-action` 处理，**不保证**透传成字面 BEL。故铃声在 tmux 下属尽力而为，回合结束的**可靠信号以 tab_status 跃迁为准**（这正是 §3.2 状态事件刷新的依据）。
- **resize 与 tmux**：控制器 `master.resize()` 会让自己这个 tmux client 触发 SIGWINCH，tmux 据 `window-size` 策略调整 pane；不要写 `\x1b[8;..t`。

### 2.2 关于 master 句柄
`PtySession` 必须**保留 `master`**：`writer`（写 tmux-client stdin）、`reader`（读 tmux-client stdout，`try_clone_reader`）、`master`（用于 `resize()`）、`child`（tmux client 进程，监测退出）。resize 通过 `master.resize(PtySize{rows,cols,..})` 实现，**绝不**往 PTY 写 `\x1b[8;..t`。

## 3. 三条数据通道

| 通道 | 数据源 | 出站消息 |
|------|--------|----------|
| 一 · 终端画面 | PTY stdout | `{"type":"output","raw":"…"}`（见 §3.1 编码） |
| 二 · 对话内容 | JSONL 文件 | `{"type":"transcript","message":{…}}` |
| 三 · 状态事件 | OSC 序列 | `{"type":"event","event":"…", …}`（见 §3.3 schema） |

### 3.1 通道一 · 终端画面

PTY 输出是二进制，UTF-8 多字节会被切在读边界。**不使用 `from_utf8_lossy`。**

**方案 A（默认，保持线缆兼容）— 尾字节缓冲**
维护 `pending: Vec<u8>`。每次读到 `buf`，拼到 `pending`，用 `str::from_utf8` 找最长合法前缀，发其 UTF-8 字符串；把不完整的尾字节留在 `pending`。ANSI 控制字节全 ASCII，绝不被截，安全无损。`raw` 仍是合法 UTF-8 字符串，Dashboard 零改动。

**方案 B（可选，最稳）— base64**
`{"type":"output","enc":"base64","data":"…"}`，Dashboard 端 `term.write(atob(data))`。彻底规避编码问题，但需前端加一行解码。

> v1 用方案 A（兼容优先）；若发现异常字节流再切 B。两者可由 CLI flag 切换。

### 3.2 通道二 · 对话内容

**路径与定位**（核对 Claude Code 源码 `utils/sessionStorage.ts` / `sessionStoragePortable.ts`）：

```
<base>/projects/<sanitized-cwd>/<session-uuid>.jsonl
base = $CLAUDE_CONFIG_DIR ?? ~/.claude
sanitized-cwd = cwd.replace(/[^a-zA-Z0-9]/g, '-')   // 每个非字母数字字符→'-'；超 200 字节再附 hash
```

- session-uuid 是启动时 `randomUUID()` 生成的，**外部无法预先固定**（`--session-id` 受 KAIROS 门控，不可用），所以文件名拿不到先验值。
- **确定性锁定（替代 mtime 抢最新）**：控制器先按 cwd 推出 `<sanitized-cwd>` 目录，**记录启动前已存在的 `*.jsonl` 集合**；启动 claude 后，该目录里**新出现的那个 `*.jsonl` 即本会话**，锁定它。避免「全局 mtime 倒序」在多 session 并发时的抖动。
- **要跳过的同目录文件**：`<sid>/subagents/agent-*.jsonl`（子代理转写，在子目录里）、`*.meta.json`、`*timeline*`、`bridge-pointer.json` 等非主转写文件。v1 只 tail 主 `*.jsonl`（子代理转写可作 v2）。
- 每行一条结构化消息（`type`: `user`/`assistant`/`system`/`summary`…，含 `uuid`/`parentUuid`/`timestamp`/`message`；`tool_use.input`、`tool_result.content` 与 Agent SDK 同构），原样转发为 `transcript`。

**增量 tail（修正半行丢失）**：写入是**缓冲、~100ms 刷盘、无 fsync**，磁盘上可能存在未闭合的半行 —— 故游标按「**最后一个换行符之后**」推进，绝不按文件长度推进。

```
last_offset = 0
on (poll tick | refresh 信号):
    len = metadata(path).len()
    if len < last_offset: 文件被截断/换 session → 重新定位，last_offset = 0
    if len > last_offset:
        seek(last_offset); read 到 EOF 进 buf
        切出 buf 中最后一个 '\n' 之前的完整部分，逐行 serde_json 解析、转发
        last_offset += （最后一个换行符的位置 + 1）   // 未闭合尾行留到下次
```

**三个刷新触发源**（汇入 `refresh_tx`，因源码**无任何"新行已落盘"的带内信号**，必须靠外部驱动）：

1. **轮询**（基线）：250ms 定时 tick。简单、跨平台、保证最终一致；可选 `notify`(inotify) 降延迟。
2. **状态事件触发**（低延迟）：通道三检测到 `tab_status` 由 `Working…` 跃迁到 `Idle`/`Waiting`（=助手回合结束）或收到 bell 时，**立即**发一次 `refresh_tx`，不等下一个轮询 tick —— 回合一结束对话内容就立刻补齐。这是把通道三→通道二耦合起来的关键设计。
3. **用户手动刷新**：远程下发 `{"type":"refresh"}`（见 §4）。`scope:"tail"` 仅追读增量；`scope:"full"` 把 `last_offset` 归零、从头重发整份转写 —— 供新接入 / 失序的 Dashboard 重建对话。

### 3.3 通道三 · 状态事件

**OSC 协议**（核对 Claude Code 源码 `ink/termio/osc.ts` 等，已修正状态文本）：

- Tab 状态 `OSC 21337`：`ESC ] 21337 ; indicator=#RRGGBB ; status=<text> ; status-color=#RRGGBB ST/BEL`。`status` 实际取值是 **`Idle` / `Working…`（带省略号 U+2026）/ `Waiting`**（不是原稿臆测的 generating/approval）；indicator 颜色：idle 绿、busy 橙、waiting 蓝；清除时各字段为空。`status` 文本里的 `;`/`\` 会被转义（`\;`、`\\`），解析需还原。
- 进度条 `OSC 9 ; 4`（iTerm2）：`ESC ] 9 ; 4 ; <op> ; <pct> ST/BEL`，op：0 清除 / 1 设置 / 2 错误 / 3 不确定。
- 标题 `OSC 0`（SET_TITLE_AND_ICON）：`ESC ] 0 ; <title> ST/BEL`。（另有 `OSC 7` = SET_CWD，可选解析以获知工作目录变化。）
- 通知铃 `BEL (0x07)`：任务完成 / 权限请求超时等；**裸 BEL，不被包裹**（tmux 下透传不保证，见 §2.1）。
- 终端专属通知（可识别但通常忽略）：iTerm2 `OSC 9`、Kitty `OSC 99`、Ghostty `OSC 777`。
- **终止符**：多数终端用 `BEL (0x07)`；**Kitty 用 `ST = ESC \`**。解析两者都要支持。
- **多路复用包装**：tmux → `ESC P tmux ; <内部 ESC 翻倍> ESC \`；GNU screen(`$STY`) → `ESC P <原样> ESC \`（**不翻倍**）。注意 §2.1：tmux 开 `allow-passthrough` 后控制器读到的多是**已解包**的裸 OSC，但解析器仍须保留 DCS 路径以兼容 screen / 直连真终端的场景。

**状态机实现要点**：整机在 **`&[u8]`** 上运行，缓冲用 `Vec<u8>`，字段提取时才 `str::from_utf8`。状态：`Ground` / `Esc`（见过 ESC）/ `Osc`（ESC ]，累积到 `BEL` 或 `ST=ESC\`）/ `Dcs`（ESC P，累积到 `ESC\`）。

- OSC 终止后，剥掉前缀 `ESC ]` 与终止符（BEL 或 ST），按首个 `;` 切出命令号，分派 0/2、9、21337。
- DCS 终止后：tmux → 剥前缀 `\x1bPtmux;`、后缀 `\x1b\\`，把内部 `\x1b\x1b` 还原为 `\x1b`；screen → 剥 `\x1bP`/`\x1b\\`、不还原；再把内层字节喂回 OSC 解析。
- 字节按 8KB 块喂入，状态跨块保留，避免任何按字节切片 panic。
- **回合结束钩子**：解析出 `tab_status` 从 `Working…` 跃迁到 `Idle`/`Waiting`、或收到 bell 时，除发 Event 外，同时向 `refresh_tx` 发一次信号触发通道二刷新（§3.2）。

**出站 Event 消息 schema**：定义单一结构体，扁平 `Option` 字段：

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

线缆示例：`{"type":"event","event":"tab_status","status":"Working…","indicator":"#ff9500"}`。

## 4. 入站协议（远程 → PTY）

```json
{"type":"input","text":"帮我重构这个模块"}     // 追加 \r 后写入
{"type":"raw","text":""}                  // Ctrl+C； Ctrl+D；[A ↑
{"type":"resize","cols":200,"rows":50}          // → master.resize()
{"type":"refresh","scope":"tail"}               // 手动刷新通道二：tail=补增量 / full=从头重发
```

- `input` 在文本后补 `\r`；`raw` 原样写入控制字符；`resize` 调 `master.resize()`（**非**写转义序列）。以上经 `pty_writer` 单任务串行执行。
- `refresh` 不进 PTY，而是向 `refresh_tx` 发信号驱动 `jsonl_watcher` 重读（§3.2）；`scope:"full"` 时把 `last_offset` 归零、从头重发整份转写，供新接入 / 失序的 Dashboard 重建对话。

## 5. 安全

**威胁模型**：控制器 = 远程 shell 注入能力 + 宿主机 `ANTHROPIC_API_KEY` 访问。任何能连上 WS 端点的人即可驱动该机器。因此鉴权是必需项，不是可选项。

- **鉴权**：连接时 `Authorization: Bearer <token>` 头或首帧 `{"type":"auth","token":"…"}`；token 来自环境变量 `CONTROL_TOKEN`，未设置则**拒绝启动**。校验失败立即关闭连接。
- **传输**：生产强制 `wss://`（rustls）。`ws://` 仅允许 `127.0.0.1` / 显式 `--insecure`。
- **最小权限**：以非 root 运行；env 注入跳过空值；不打印 token/key。
- **可选**：入站命令审计日志、来源 IP 允许列表。

## 6. 依赖

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

## 7. 背压与重连

- `out_tx` 用**有界** channel（容量 1024）。WS 已连：正常发送。WS 断开：`ws_outbound` 不消费，channel 满后 `try_send` 失败 → 对**通道一 output** 采用「丢旧」策略（终端画面是可重建的最终态），对**通道二/三**尽量保序不丢（容量内）。
- **重连快照**：v1 标注限制——新接入只见后续输出。v2 引入 `vt100` 维护屏幕缓冲，新连接先发一帧 `{"type":"snapshot","screen":"…"}`，再转入实时流。
- WS 重连：指数退避（2s 起，封顶），重连后重新鉴权。

## 8. 生命周期

- 启动：解析 env/flag → 校验 `CONTROL_TOKEN` → 记录项目目录现有 `*.jsonl` 集合（§3.2 会话锁定）→ 起 PTY 跑 `tmux new-session -A`（§2.1）并设 `allow-passthrough on` → 锁定新出现的 jsonl → 起各 task → 连 WS。
- 关闭：`SIGINT`/`SIGTERM` 触发 `CancellationToken`；各 task 收尾；**控制器 detach（不 kill）**——tmux 会话与 claude **继续后台留存**，下次以同名 `new-session -A` 接管。
- 仅当显式 `--shutdown-session` 时才 `tmux kill-session` 真正结束 claude。
- tmux client 退出（reader EOF）：视为 detach；若进程整体退出则优雅关闭。

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

# 前置依赖：服务器需装 tmux。本地随时接入同一会话（双向透明）：
tmux -L claude-ctl attach -t claude-ctl
```

## 10. 与稿件的差异小结

| 项 | 原方案 | 本设计 | 原因 |
|----|--------|--------|------|
| 并发模型 | 手写多线程 + 裸流克隆 | tokio + `.split()` | 裸流克隆绕过 WebSocket 帧封装/客户端掩码，连接被按协议错误关闭 |
| TLS | 无 feature，仅 ws | rustls-tls-webpki-roots | 原配置无法 `wss://`，与「无 OpenSSL 安全部署」矛盾 |
| 通道一编码 | `from_utf8_lossy` | 尾字节缓冲 / base64 | lossy 在 8KB 读边界切坏多字节字符 → 乱码 |
| resize | 写 `\x1b[8;..t` | `master.resize()` | 写转义序列对内核 PTY 无效，子进程收不到 SIGWINCH |
| Event 类型 | 枚举 + `..` | 扁平 `Option` 结构体 | 枚举结构体变体不能用 `..` 填充，编译不过 |
| JSONL 游标 | 文件长度 | 末换行偏移 | 按长度推进会把未闭合的半行永久丢失 |
| 鉴权 | 无 | 强制 token + wss | 无鉴权 = 远程 shell 注入，RCE 级风险 |
| OSC 状态机 | `byte as char` String | `&[u8]` | 按字节切片遇非字符边界会 panic；tmux DCS 解包偏移错 |
| 背压 | 无界 channel | 有界 + 丢旧 | 断连期间无界堆积，重连后灌洪 |
| 会话承载 | PTY 直接 spawn claude | **tmux `new-session -A`** | 后台留存 + 本地 `attach` 双向透明，控制器可重启接管 |
| 通道二刷新 | 仅 500ms 轮询 | 轮询 + **状态事件触发** + **手动 refresh** | 源码无落盘带内信号；回合结束靠 tab_status 跃迁低延迟补齐 |
| OSC 直通 | 未涉及 | tmux `allow-passthrough on` | 否则 claude 的 DCS 包裹 OSC 被 tmux 丢弃，收不到状态事件 |
| status 取值 | 臆测 generating/approval/idle | 实测 `Idle`/`Working…`/`Waiting` | 与源码 `use-tab-status.ts` 对齐 |
| jsonl 定位 | 全局 mtime 抢最新 | 项目目录 + 启动前后 jsonl 差集锁定 | 路径 `[^a-zA-Z0-9]→-`，避免多 session 抖动 |

## 11. 里程碑

1. **M1 骨架**：PTY 跑 tmux `new-session -A` + `allow-passthrough` + 通道一（尾字节缓冲）+ ws_outbound，本地 `ws://127.0.0.1` 跑通画面。
2. **M2 双向**：入站 input/raw/resize（`master.resize`）+ 鉴权 + wss；验证本地 `tmux attach` 与远程同时透明驱动。
3. **M3 通道二/三**：jsonl 差集锁定 + tail（末换行游标）+ OSC 状态机（`&[u8]` + tmux/screen 解包）+ 三源刷新（轮询 / 状态事件 / 手动）。
4. **M4 健壮性**：有界背压 + 重连退避 + 优雅 detach（保留会话）+ tmux client 退出处理。
5. **M5（可选）**：vt100 重连快照、notify、base64 模式、子代理转写、审计日志。
