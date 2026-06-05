# claude-pty-controller — 架构设计

> 把一台运行 Claude Code 的机器，通过单一静态 Rust 二进制暴露为可远程观测 / 驱动的终端会话。
> 本文是在初版方案 `方案6-Rust版.md`（源文件已删除,保留在 git 历史中）基础上、修正其若干缺陷后的落地设计；与原方案的关键差异及原因汇总见 §10。

> ⚠️ **版本敏感性**：本文对 Claude Code 内部细节的论断，关键面已**对环境实际安装的 claude v2.1.163 实测核对**（`claude agents --json`、`~/.claude/sessions/*.json`、`daemon/roster.json`、`projects/<sanitize(cwd)>/<sessionId>.jsonl`、二进制里 OSC `21337` 与 `Ptmux;` 仍在、status 文本非 "generating"，见 §3.5）。其余细节核对自一个**可能偏旧**的源码快照。设计有意**架在跨版本稳定的不变量上**（PTY 字节流、行内 `sessionId`/`cwd` 戳、tmux 自省、文件系统观测），易变细节降级为「实现时按**目标 claude 版本**现场复核」—— 变了只需调参数、不动架构。

## 1. 目标与非目标

**目标**
- 单一静态二进制（`scp` 即部署，无运行时依赖）。
- 三条数据通道，统一走 **§16.3 规范化线缆 schema**（带 `v` 版本字段协商；§3/§3.3/§4 展示的是 Claude 原生载荷，承载在规范消息的 `raw`/`parts` 里）。**注意：这是规范 schema、非与任何既有 Dashboard 线上兼容**——前端按 §16.3 实现。
- 远程可观测（终端画面 + 结构化对话 + 状态事件）且可驱动（输入注入 / 控制字符 / resize）。
- **会话后台留存**：claude 跑在 **tmux** 会话里，控制器 / SSH 断开不影响 claude 继续运行，控制器重启后无缝接管同一会话。
- **本地双向透明**：本地终端可随时 `tmux attach` 接入同一会话，与远程 Dashboard **同时、透明**地观看并驱动同一个 claude（tmux 原生多客户端镜像）。
- **对话内容低延迟**：通道二除轮询外，由**状态事件触发**和**用户手动刷新**两条额外路径驱动重读（见 §3.2）。
- 默认安全：强制鉴权，支持 `wss://`（rustls，无 OpenSSL）。
- **三端 + 端到端加密**：被控端 / 中转端 / 控制端三端模型（§13）；中间流量对中转端**零知识**（E2EE，§14）——relay 只转发不透明密文，读不到内容；**每设备静态密钥身份、可吊销**。
- **Agent 无关（适配器层）**：核心与具体 agent 解耦，新 agent TUI 只需写一个 `AgentAdapter`（§16）；通道一对任何 TUI 零-adapter 可用，Claude 是首个 adapter。

**非目标（v1）**
- 多会话结构化 fan-out（v1 通道一即透明覆盖单前台会话；后台 agents 的并发 tail + 状态列表为 v2，已实测可行，见 §3.5）。
- 录制回放、持久化历史（仅内存快照）。
- 原生 Windows 的**会话宿主**（v1 经 **WSL2** 即获完整能力；原生 ConPTY 宿主为 v2/v3，§15）。其余能力（通道二/三、多会话、E2EE、relay）本就跨平台。

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
tmux -L claude-ctl set -g allow-passthrough all  # 'on' 仅在 pane 可见时直通；无头控制器窗口可能不可见 → 用 'all' 避免静默丢 OSC（实测：默认 off，必须显式开）
tmux -L claude-ctl set -g status off              # 关状态栏：否则控制器读到的整屏含状态栏+时钟，时钟每秒重绘灌入通道一（实测）
tmux -L claude-ctl set -g pane-border-status off  # 关 pane 边框标题
tmux -L claude-ctl set -g window-size latest      # latest 本是 tmux 默认；显式写明（见下「尺寸权威」取舍）
```

开启 passthrough 后 tmux 会**解包**内层序列再转发给每个 client 的终端 —— 即控制器 PTY 读到的是**已解包的裸 OSC**（实测 tmux 3.4 成立）。但状态机仍需保留 DCS 解包路径，以兼容非 tmux / GNU screen（`$STY`）场景（§3.3）。
- **通道一是 tmux client 整屏（实测，重要）**：控制器 PTY 是个 tmux *client*，渲染的是**整窗**——含 tmux 状态栏 / 分屏边框 / 每秒时钟重绘。故必须 `status off`（上）；否则通道一携带 tmux chrome 且空闲也持续产出 output（灌入有界 out_tx，§7）。
- **铃声 caveat**：claude 发的是**裸 BEL**（未包裹）；tmux 按 `bell-action` 处理。实测默认 `bell-action any` **会**透传一个 0x07，但配置相关（`visual-bell on` / `bell-action none` 会改变），故仍属尽力而为，回合结束的**可靠信号以 tab_status 跃迁为准**（§3.2 状态事件刷新的依据）。
- **尺寸权威（多客户端取舍，实测）**：tmux **一个 window 的所有 client 共享同一 pane 几何**，无 per-client 尺寸。`window-size latest` 下"最后 attach/resize 的 client 赢"——本地 attach 与远程 `master.resize()` 会**互相夺屏**。且 `new-session -A -x/-y` 在**接管已存在会话时忽略 -x/-y**（实测），重连不能靠 flag 固定尺寸。**策略：控制器独占几何**——重连后用 `master.resize()`（或 `tmux resize-window -x/-y`）显式设回；本地 attach 视为"按控制器尺寸只读/letterbox"。`master.resize()` 触发 SIGWINCH，**绝不**写 `\x1b[8;..t`。
- **本地 attach 输入也会交错**：单 `pty_writer` 只串行化**控制器自身**的入站;本地 `tmux attach` 的击键直接进 pane stdin、绕过 pty_writer,与远程输入在 pane 处仍可能逐字交错（§13 的单-driver 锁约束不了直接 attach）。需要严格单写时,给本地 attach 用 tmux 只读模式（`attach -r`）。

### 2.2 关于 master 句柄
`PtySession` 必须**保留 `master`**：`writer`（写 tmux-client stdin）、`reader`（读 tmux-client stdout，`try_clone_reader`）、`master`（用于 `resize()`）、`child`（tmux client 进程，监测退出）。resize 通过 `master.resize(PtySize{rows,cols,..})` 实现，**绝不**往 PTY 写 `\x1b[8;..t`。

## 3. 三条数据通道

> **线上格式以 §16.3 规范 schema 为准（normative）**。本节 §3.x 是 **Claude 的具体实现（`ClaudeAdapter`）**：下表 / §3.3 / §4 里的 `{type:transcript,message:…}`、`event/status` 等是 **Claude 原生载荷**，实际承载在规范消息的 `raw`（及映射后的 `parts`/`state`）里——**不是独立的线上格式**。通道一对任何 agent 通用；通道二/三的 agent 专属部分由 §16 的 `AgentAdapter` 抽象。

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
- ⚠️ **sanitize 是多对一、会撞目录（实测）**：`/root/ws63-rs` 与 `/root/ws63_rs`、`…/.claude-worktrees` 与 `…--claude-worktrees` 映射到**同一** `<sanitized-cwd>`（后者本机实际存在）。故"同目录新增/最近增长"可能选到**另一个 cwd 的会话**。消歧以**精确 cwd**为准（从 `sessions/<pid>.json` 按 sessionId 反查，§3.5），不靠目录成员；保留 §3.2.1 的 tab_status 窗口作二次门槛。
- **要跳过的同目录文件**：`<sid>/subagents/agent-*.jsonl`（子目录里）、`<sid>/`、`memory/`（子目录，含 `*.md`）、`*.meta.json`、`*timeline*`、`bridge-pointer.json`。**精确匹配 = 顶层 stem 为 UUID 的 `*.jsonl`，不递归进子目录**。v1 只 tail 主 `*.jsonl`（子代理转写可作 v2）。
- 每行一条结构化消息，但**字段并非每行都全**（实测 v2.1.163）：`sessionId` 在除 `file-history-snapshot` 外的所有行；`cwd` **只在** `user`/`assistant`/`system`/`attachment` 行；`uuid` 同理只在消息行，`file-history-snapshot`/`last-prompt`/`mode`/`ai-title`/`queue-operation` 等辅助行**无 uuid**。→ **转写通道只转发带 `uuid` 的消息行**（user/assistant/system），辅助行不转发，避免下游无 key 可去重（§12.4）。

**增量 tail（修正半行丢失）**：写入是**缓冲、~100ms 刷盘、无 fsync**，磁盘上可能存在未闭合的半行 —— 故游标按「**最后一个换行符之后**」推进，绝不按文件长度推进。

```
last_offset = 0; locked = (path, inode)          // 记住身份，不只记 path
on (poll tick | refresh 信号):
    if (path, inode) != locked:                   // 文件被替换/换 session（同长或更长也能抓到）
        locked = (path, inode); last_offset = 0   // 身份变即重锚（覆盖 in-place rewrite，单纯 len< 测不出）
    len = metadata(path).len()
    if len < last_offset: last_offset = 0          // 截断兜底
    if len > last_offset:
        seek(last_offset); read 到 EOF 进 buf
        切出 buf 中最后一个 '\n' 之前的完整部分，逐行 serde_json 解析、转发
        last_offset += （最后一个换行符的位置 + 1）   // 未闭合尾行留到下次
```
> 仅 `len < last_offset` 只能测出截断；/resume 切到**更长**的已存在文件、或 inode 原地替换都测不出 → 必须按 `(path, inode)` 身份变化重锚（实测必要）。

**三个刷新触发源**（汇入 `refresh_tx`，因源码**无任何"新行已落盘"的带内信号**，必须靠外部驱动）：

1. **轮询**（基线）：250ms 定时 tick。简单、跨平台、保证最终一致；可选 `notify`(inotify) 降延迟。
2. **状态事件触发**（低延迟）：通道三检测到 `tab_status` 由 `Working…` 跃迁到 `Idle`/`Waiting`（=助手回合结束）或收到 bell 时，**立即**发一次 `refresh_tx`，不等下一个轮询 tick —— 回合一结束对话内容就立刻补齐。这是把通道三→通道二耦合起来的关键设计。
3. **用户手动刷新**：远程下发 `{"type":"refresh"}`（见 §4）。`scope:"tail"` 仅追读增量；`scope:"full"` 把 `last_offset` 归零、从头重发整份转写 —— 供新接入 / 失序的 Dashboard 重建对话。

#### 3.2.1 运行时会话切换识别（/resume、新开、切换）

用户可能在 pane 里 `/resume`、`claude --resume`、或退出后重开 claude —— 此时**活跃 JSONL 会变**：核对源码，resume 走 `switchSession(newSid, projectDir)` 后**追加写到被恢复会话已有的 `<uuid>.jsonl`**，且 `projectDir` 可能**指向另一个项目目录**。启动时锁定的单文件就此失效，必须运行时重新识别。

可用信号按可靠性排序：

1. **每行的 `sessionId`（主信号，但非每行都有）**：除 `file-history-snapshot` 外的行都带 `sessionId`（实测；`cwd` 只在消息行）。读到带 `sessionId` 的行,**值与当前追踪不同 = 切换**。⚠️ **必须跳过无 `sessionId` 的行**（如交错其间的 `file-history-snapshot`，实测一个 905 行文件里有 36 条）——把"缺失"当切换会**误判 → 假 session 事件 + 历史灌洪**；应向后找最近带 `sessionId` 的行。`cwd` 缺失时从 `sessions/<pid>.json`（按 sessionId）反查，不靠行内。
2. **活跃文件 = 当前正在增长的 `*.jsonl`（文件发现，实际首发信号）**：实测同一文件内 `sessionId` 恒等于文件名 UUID，故 #1 的"行内 sessionId 变"**只在已读到另一个文件后**才成立——真正先触发的是**文件发现**，行内 `sessionId` 是"确认没认错文件"的护栏。每个 poll **重跑活跃文件发现**，非锁定文件开始增长即切目标。
3. **OSC 0 标题变化（次要佐证）**：切换会重发 `ESC]0;<title>`（§3.3），可作为"可能切换"的提示去主动复扫；但标题为空时不发，不能单独依赖。
4. ~~OSC 7 cwd~~：源码**不发** OSC 7，cwd 只在 JSONL 行里 —— 拿不到终端信号。

**识别 + 重锁流程**：
```
每个 poll / 收到标题变化提示:
    scope = 当前 cwd 的项目目录（若锁定文件转静默且 scope 内无增长 → 回退扫全 projects/ 树）
    active = scope 内最近在增长的顶层 *.jsonl（stem=UUID；排除 subagents//memory/ 子目录、*.meta.json、timeline、bridge-pointer）
    读 active 中最近一条【带 sessionId】的行（跳过 file-history-snapshot 等无 sessionId 行）:
        active == 锁定文件(path,inode) → 正常 tail（§3.2）
        active 是别的文件 → 会话切换：
              current_sid = 该行 sessionId；锁定 active；last_offset = 0
              cwd = sessions/<pid>.json[sessionId].cwd（行内可能没有）
              发 {"type":"session", sessionId, cwd, path, reason:"resume|new|switch"}
              从头 tail 新文件
```

**多 claude 实例消歧**：同机若有别的 pane 在写 jsonl，"全局最近增长"可能误指他人会话。用**本 pane 的 `tab_status` 活动窗口**做归属门槛 —— 仅当某文件**在本 pane 处于 `Working…` 期间**增长、或其行 `sessionId` 与既有追踪一致时，才采纳为本会话。把通道三的活动状态耦合进通道二的发现，避免抢错。

**取舍**：切换时 `last_offset=0` 会**重发被恢复会话的全部历史**（resume 文件本含历史）—— 对前端是"完整呈现恢复后的对话"，但大历史有一次性灌洪，靠有界背压（§7）+ 前端按 `uuid` 去重消化。**前提**：去重 key `uuid` 只在消息行存在（见 §3.2），故转写通道**只转发带 uuid 的消息行**——辅助行无 uuid、无法去重，不转发。新增出站控制消息 `{"type":"session",…}` 让前端在切换点清空/重建对话视图。

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
- **健壮性**：① 状态匹配按 `indicator` 颜色 / `status` 前缀（`Working` 含 U+2026 省略号,匹配前缀而非精确字面,避免编码/版本漂移）;② 只认 OSC `21337`/`9;4`,不被 pane 内**其他程序**的 OSC（如别的程序设标题/进度）误触发——尤其 OSC `0` 标题来源不限于 claude,**不能**单独据它判会话切换（§3.2.1 已降级为次要佐证）;③ CSI（`ESC [ …`）等非 OSC/DCS 序列在状态机里走 Ground、不误入 OSC。
- **回合结束钩子**：解析出 `tab_status` 从 `Working…` 跃迁到 `Idle`/`Waiting` 时，除发 Event 外，同时向 `refresh_tx` 发一次信号触发通道二刷新（§3.2）。bell 仅作尽力而为提示、不作权威回合界（§16.3 ADP-6）。

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

### 3.4 模式无关性与优雅降级（含 `claude agents` / 全屏 UI）

claude 在 pane 里会进入各种非常规对话状态：`/resume` 选择器、plan 模式、`/agents`（REPL 内配置 UI）、以及 **`claude agents`**（**实测 v2.1.163**：打开一个**交互式 agent view**，派发/监控 daemon 托管的后台 agent 会话 —— 不是旧源码的"列出即退"；多会话细节见 §3.5）。设计原则：

- **通道一（屏幕）模式无关、永远可用**：tmux 镜像的是 pane 的最终 ANSI 画面 —— 无论 claude 渲染的是对话、全屏 alt-buffer UI、还是列表输出，远程 Dashboard + 本地 attach 都**透明可见可驱动**。`claude agents` / `/agents` 这类模式**仅靠通道一即可完整远程操作**。
- **通道二/三 best-effort、优雅降级**：这些模式不产生转写、不发状态 OSC 时，通道二自然**安静**、通道三仅余 idle 跃迁 —— 这是**预期降级，不是故障**：没转写就不发 transcript，没 OSC 就不发 event，通道一照常。控制器不得因通道二/三无数据而误判会话异常。
- **回到对话由 §3.2.1 兜底**：从 `/agents` 选中并运行某 agent → 进入正常会话（同一 `<sessionId>.jsonl`，通道二恢复）或派生子代理；会话切换/恢复一律由按行 `sessionId` 重锁覆盖。

> **子代理（Task fan-out）**：其内部对话写在独立 sidechain `…/<sessionId>/subagents/agent-<agentId>.jsonl`，主转写只含 `Task` 的 tool_use + 最终 tool_result（即"看得到 agent 的输入与最终产出，看不到内部步骤"）。v1 通道二**仅 tail 主文件**；**多文件嵌套 tail 子代理对话**列为 v2（§11）—— 届时并发 tail 各 sidechain（每文件独立换行游标），按 `agentId` / `isSidechain` / `parentUuid` 还原嵌套树，并按主转写里 `Task` tool_use.id ↔ `agent-<id>` 关联归属。远程 agents（CCR）只有 `remote-agents/*.meta.json`，转写在云端、本地不可 tail。

### 3.5 多会话 / 后台 agents（一终端管多 session）—— 实测 v2.1.163

已对**环境中实际安装的 claude v2.1.163** 验证：所谓"一终端管多 session"是 `claude agents` 打开的 **agent view**，它**派发并监控由 daemon 托管的后台 agent 会话**——**不是**旧源码里的 tmux 分屏 / teams（本机 `~/.claude/teams/` 为空）。后台 agent 是**无前台终端的 daemon worker 进程**，各有独立 `<sessionId>.jsonl` 与状态。

**实测可用的发现面（按优先级，均无需解析合流字节）**：

1. **`~/.claude/sessions/<pid>.json`（首选，直接读 / inotify）** —— 每个活跃会话一份：
   `{pid, sessionId, cwd, kind:"interactive"|"bg", status:"busy"|…, name?, jobId?, updatedAt, entrypoint, version, bridgeSessionId}`。读目录即得全部会话 + 状态；**以本面的 `updatedAt` 判活性**（实测最新）。
2. **`claude agents --json`（官方脚本接口）** —— 免 TTY 打印 live sessions 数组并退出：`{pid, cwd, kind, sessionId, status, name?}`。⚠️ **实测 ~0.39s / ~300MB RSS 每次调用**——**绝不放进 250ms 轮询**，仅作冷启动 / 低频交叉校验。
3. **`~/.claude/daemon/roster.json`（后台细节，可能陈旧）** —— daemon worker（按 `jobId` 作 map 键，非字段）：含 `sessionId, cwd, pid` 及 **`ptySock`**、`rendezvousSock`、`dispatch`(cols/rows)。⚠️ 实测**可陈旧达数十分钟**、且其 `pid` 与 `sessions/*.json` 对同一 sessionId **不一致**（worker pid vs 进程 pid）——只取后台细节,活性/对账**以 `sessions/*.json` 为准、按 `sessionId` 对账**，不按 pid。
4. **文件系统兜底** —— 并发增长的 `*.jsonl` + 行内 `sessionId`（§3.2.1 的版本无关地板）；当上面格式变动时退化使用。

> ⚠️ **发现面非完全一致（实测）**：`kind` 在 `sessions/*.json` 是 `"bg"`、在 `claude agents --json` 是 `"background"` —— adapter 须**用映射 normalize**（`background→bg`），勿用字符串相等;跨面对账一律按 `sessionId`。

**会话 → 转写路径（实测确认）**：`~/.claude/projects/<sanitize(cwd)>/<sessionId>.jsonl`，`sanitize = [^a-zA-Z0-9]→-`（例：`/root/ws63-rs` → `-root-ws63-rs`）。

**据此分通道**（通道一 v1 已覆盖前台；多会话 fan-out 为 v2）：

- **通道二（多会话并发 tail）**：从发现面拿到会话集合 → **按 `sessionId` → 精确 `<sessionId>.jsonl` 路径** 打开（**不要**用"目录内最近增长的文件"做归属——后台 agent 无 tab_status 窗口可消歧，§3.2 sanitize 又会撞目录）→ 并发 tail。⚠️ **大文件灌洪（评审）**：后台 agent 转写实测可达 **59MB**;首次锁定**别从 offset 0 重放全历史**(超 1024 槽有界 channel 且会 head-of-line 阻塞前台 tail) → 首锁 seek 到近 N 行/分页回填,后台回填走**低优先级独立 lane**,历史按需由 Dashboard `refresh:full` 拉。
- **通道三（状态）**：⚠️ **`status:busy` ≠ 回合进行中（评审,实测）**:`sessions/*.json` 的 `status` 是**进程是否在工作**,与回合边界不同轴(同一 agent `status:busy` 而其 `jobs/<id>/state.json` 已 `done`)。故后台 agent **没有"回合结束"边沿信号**,§3.2 路径 2 的低延迟刷新对它**不适用** → 后台通道二退化为**轮询**,或"回合结束"改由**通道二内容自身**判定(新 assistant 行落盘)。前台仍走 OSC `tab_status`。
  - ⚠️ **活性不能只看 `updatedAt`（评审,实测）**:`agents --json` 之所以可信是它对每个 pid 做了 `process.kill(pid,0)/ESRCH` 探活,而 `sessions/<pid>.json` 仅在**优雅退出**时删除 —— 崩溃/OOM 会留下**陈旧 pid 文件**,fan-out 会把死 agent 当活的一直 tail,且 pid 复用会张冠李戴。→ 发现循环须**自己对每个 pid `kill(pid,0)` 探活** + `updatedAt` 陈旧阈值,按 `sessionId`(非 pid)对账。
- **通道一（屏幕）**：前台交互会话 = 控制器的 tmux pane（如常）。⚠️ **后台 agent 并非"无屏"（评审,实测）**:它跑在一个 **daemon 持有的真实 PTY**（`/dev/pts/*`,229x64,全鼠标+bracketed-paste DEC modes）上、照发同样的 OSC,只是没接到控制器的 pane —— 经 `ptySock` 即可取其屏与原生 OSC。v1 对后台只用"转写+状态"是**有意的范围裁剪,不是信号不存在**。
  - *v2 探索（不依赖）*：连后台 agent 的 `ptySock`（其属主是 `--bg-pty-host` **父进程**,pid 与 `sessions/*.json` 的 claude **子进程** pid 不同,实测 PPID 关系）即可流式取/驱其屏,与 tmux attach 对称。daemon 内部协议(`proto:1`)版本敏感、未公开,仅探索。

> 本节为**实测 daemon + `claude agents --json` + `~/.claude/sessions/*.json`** 的确定面（round-2 评审已修正"后台无屏 / status=回合 / updatedAt=活性"三处误述）。即便未来发现面变,§3.2.1 的"按 `sessionId` 精确取文件"仍是地板。

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
- **三端与令牌拆分**：本节针对**直连**（dashboard 直连被控端 wss）。经中转端时升级为三端模型（§13）+ 端到端加密（§14）：`CONTROL_TOKEN` 升格为 E2EE 根信任 `PAIRING_SECRET`（relay 不可见），relay 接入另用可选 `RELAY_TOKEN`。

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
# E2EE（§14）：snow = "0.9"（Noise IK/XXpsk3 + X25519，纯 Rust）、hmac + sha2（room id / HKDF）；静态密钥与授权表持久化（serde_json + 文件 0600）

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

**TLS 走 rustls + webpki-roots，无 OpenSSL（实测：解析后依赖树 0 个 openssl crate）。** 但**并非"无 C / 完全静态"**：tokio / portable-pty 链接 **libc（glibc）**，二进制是动态链接 glibc 的**单一自包含可执行文件**，非纯静态（要纯静态需 `x86_64-unknown-linux-musl` 目标）。体积：带 tokio + rustls + portable-pty 的 release 现实约 **8–15MB**（非早期稿的"数 MB / ~3MB"），`scp` 即部署。
> ⚠️ **snow 目前仅设计、尚未加入 `Cargo.toml`**（E2EE 依赖未验证可编译）。§14 落地前需真加 `snow`/`hmac`/`sha2` 并 `cargo check`，确认 `Noise_IK` / `Noise_XXpsk3` 在 snow 0.9 受支持。

## 7. 背压与重连

- `out_tx` 用**有界** channel（容量 1024）。WS 已连：正常发送。WS 断开：`ws_outbound` 不消费，channel 满后 `try_send` 失败 → 对**通道一 output** 采用「丢旧」策略（终端画面是可重建的最终态），对**通道二/三**尽量保序不丢（容量内）。
- **重连快照**：v1 标注限制——新接入只见后续输出。v2 引入 `vt100` 维护屏幕缓冲，新连接先发一帧 `{"type":"snapshot","screen":"…"}`，再转入实时流。
- WS 重连：指数退避（2s 起，封顶），重连后重新鉴权。

## 8. 生命周期

- 启动：解析 env/flag → 校验 `CONTROL_TOKEN` → 记录项目目录现有 `*.jsonl` 集合（§3.2 会话锁定）→ 起 PTY 跑 `tmux new-session -A`（§2.1）并设 `allow-passthrough all` / `status off`（§2.1）→ **接管已存在会话时用 `master.resize()` 显式设回几何**（`-x/-y` 实测在接管路径被忽略）→ 锁定新出现的 jsonl → 起各 task → 连 WS。
- 关闭：`SIGINT`/`SIGTERM` 触发 `CancellationToken`；各 task 收尾；**控制器 detach（不 kill）**——tmux 会话与 claude **继续后台留存**，下次以同名 `new-session -A` 接管。
- 仅当显式 `--shutdown-session` 时才 `tmux kill-session` 真正结束 claude。
- **tmux client EOF（reader EOF）必须先消歧**（实测：`kill-session` 与 `detach` 在 EOF 层**无法区分**，都是干净 EOF / 退出码 0）：收到 EOF 后查 `tmux -L claude-ctl has-session -t claude-ctl` —— 会话**仍在** → detach，重 attach；**已无** → **默认退出（尊重带外 `kill-session`）**,仅当显式 `--recreate-on-missing` 才重建。⚠️ 否则 `Restart=always` + `new-session -A` 会**把操作者刚带外杀掉的会话复活**（评审）——权威停止路径应是 `systemctl stop`（SIGTERM=detach、claude 留存）,真要结束 claude 用 `--shutdown-session`。

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
| OSC 直通 | 未涉及 | tmux `allow-passthrough all` + `status off` | 否则 DCS 包裹 OSC 被丢/被 chrome 污染（`on` 仅 pane 可见时直通；§2.1） |
| status 取值 | 臆测 generating/approval/idle | 实测 `Idle`/`Working…`/`Waiting` | 与源码 `use-tab-status.ts` 对齐 |
| jsonl 定位 | 全局 mtime 抢最新 | 项目目录 + 启动前后 jsonl 差集锁定 | 路径 `[^a-zA-Z0-9]→-`，避免多 session 抖动 |
| 会话切换识别 | 未涉及 | 追踪每行 `sessionId` + 活跃文件复评 | /resume 会换文件（甚至换项目目录），靠行内 `sessionId` 唯一真值重锁（§3.2.1） |
| 系统拓扑 | 仅被控端 | 被控 / 中转 / 控制 三端 | 双向外连穿透 NAT、多控制端扇出（§13） |
| 中间加密 | 仅 wss（relay 见明文） | wss + E2EE（Noise），relay 零知识 | 中转端不可信，流量须对其不可读且仍透明转发（§14） |

## 11. 里程碑

1. **M1 骨架**：PTY 跑 tmux `new-session -A` + `allow-passthrough` + 通道一（尾字节缓冲）+ ws_outbound，本地 `ws://127.0.0.1` 跑通画面。
2. **M2 双向**：入站 input/raw/resize（`master.resize`）+ 鉴权 + wss；验证本地 `tmux attach` 与远程同时透明驱动。
3. **M3 通道二/三（= `ClaudeAdapter`，§16 适配器层首个实现）**：jsonl 差集锁定 + tail（末换行游标）+ **会话切换识别（按行 `sessionId` 重锁，§3.2.1）** + OSC 状态机（`&[u8]` + tmux/screen 解包）+ 三源刷新（轮询 / 状态事件 / 手动）+ 出站走规范 schema（§16.3）。
4. **M4 三端 + E2EE**（§13/§14）：`relay` binary（room 撮合 + 不透明帧转发）+ 被控端外连 relay + **静态密钥身份**（稳态 `Noise_IK` + 入网 `Noise_XXpsk3`，`snow`）+ **授权设备表 / 吊销**（热加载、踢在用会话）+ AEAD 帧 + `room id` 派生；验证 relay 抓包仅见密文、吊销设备即刻失联。
5. **M5 健壮性**：有界背压 + 重连退避（重连即重握手）+ 优雅 detach（保留会话）+ tmux client 退出处理 + **单实例 / 单 driver 锁**（§12.4）。
6. **M6（v2 / 可选）**：**后台 agents 多会话 fan-out**（发现面 `sessions/*.json` / `agents --json` + 并发 tail + 免-OSC 状态，§3.5）+ 子代理嵌套 tail（§3.4）+ vt100 重连快照 + notify + base64 + 审计日志 + systemd watchdog（§12）+ **原生 Windows `ConPtyHost`**（§15，v1 已可经 WSL2 运行）+ **`GenericAdapter` + 第二个 agent adapter** 验证 §16 抽象。
7. **跨阶段校验**：对照**目标 claude 版本**复核版本敏感常量（OSC/status、JSONL 路径与字段、发现面 schema）—— 见顶部「版本敏感性」。

## 12. 后台常驻 / 进程托管

### 12.1 两层常驻，分别看

| 层 | 谁负责 | 现状 |
|----|--------|------|
| **claude 会话** | tmux `new-session -A`（§2.1） | 已常驻：控制器/SSH 断开不影响 claude 继续跑 |
| **控制器进程**（远程桥 + tail + 解析） | 外部进程守护 | **默认前台**，不自我 daemon 化 —— 见下 |

控制器是普通前台进程：SSH 断 → 收 `SIGHUP` 退出 → **远程桥断**（claude 仍活，但 Dashboard 连不上，直到控制器被重新拉起）。所以「远程随时可达」这个属性，依赖**控制器自身也常驻**。

### 12.2 自愈链（设计依据）

控制器已具备两个幂等的"接管"能力：**WS 自动重连**（§7）+ **`new-session -A` 重接管已存在的 tmux 会话**（§2.1）。因此只要外面套一层**进程守护**，三者组合成全链自愈：

```
控制器崩溃/被杀  →  守护(systemd)重启  →  new-session -A 接回还活着的 claude 会话
                                       →  WS 指数退避重连  →  Dashboard 自动恢复
```

关键前提：**重启必须能无副作用地接管**，这要求控制器满足「单实例 + 无破坏性重连」语义（§12.4）。

### 12.3 托管方案对比

| 方案 | 适用 | 优点 | 取舍 |
|------|------|------|------|
| **systemd 服务（推荐）** | 有 root 的 Linux 服务器 | 开机自启、崩溃自动重启、journald 日志、env 隔离、可选 watchdog/沙箱 | 需 root 写 unit |
| tmux/`setsid nohup` | 无 root / 临时 | 一行起，零配置 | 无自动重启、无开机自启、日志靠重定向 |
| 内建 `--daemon`（double-fork + `setsid`） | 不想依赖外部守护 | 自包含 | 重造守护逻辑、信号/日志/重启都要自己写，**不推荐**（优先交给 systemd） |

**结论**：v1 不内建 daemon 化；以 systemd 为一等公民部署方式，附 `setsid nohup` 作为无 root 兜底。控制器只需把自己做成「**对前台/被托管两种方式都正确**」的普通进程：

- 默认日志写 stderr（systemd 自动进 journald；裸跑可重定向），`RUST_LOG` 控级别，**绝不打印 token/key**。
- `SIGTERM`/`SIGINT` → 优雅 detach（保留 tmux 会话，§8），退出码 0；守护视为正常停止。
- 不写 PID 文件依赖（交给 systemd 跟踪）；单实例靠 flock（§12.4）。

### 12.4 单实例与幂等接管

若一个常驻控制器**和**一个手动拉起的控制器同时连同一会话，会出现：两个进程都 tail 同一 JSONL（转写**重复下发**）、都往同一 PTY 写（输入**交错**）。故须**单实例约束**：

- 启动时对一个**命名空间稳定**的锁文件做 `flock(LOCK_EX|LOCK_NB)`；拿不到锁 → 已有实例 → **直接退出**（让守护不起重复实例），或按 flag 改为「抢占」。⚠️ **抢占需旧实例 PID** 而 §12.3 不写 pidfile——折中是把 PID 写进**锁文件正文**（锁本身保活性,正文仅供发信号）;否则 v1 只保留"丢锁即退",由 systemd 串行化。
- ⚠️ **锁目录要避开 PrivateTmp（评审）**：系统服务常无 `XDG_RUNTIME_DIR`,回退 `/tmp`;若 systemd 开 `PrivateTmp=yes`,被托管实例与手动实例的 `/tmp` 在**不同 mount namespace**,互相看不见锁 → 单实例**静默失效**。锁应放 `RuntimeDirectory=`（`/run/<name>`,不受 PrivateTmp 影响）或显式配置目录,并在启动日志打印解析后的锁路径。
- ⚠️ **此 flock ≠ §13 的 driver 锁（评审）**：flock 只防"第二个**控制器进程**启动"(host 层 OS 锁);§13 的"单 driver"是仲裁"同一控制器的 N 个 **dashboard** 谁能写"(进程内、按设备公钥的票据)。**两者只共享"单写"原则、不是同一机制**;别以为 ship 了 flock 就有了 driver 互斥。`<session>` 当前是常量(tmux 名硬编码 `claude-ctl`),"多会话天然隔离"不成立——要么参数化会话名并同步锁键,要么明示 v1 为单会话机器级单例。
- 重连/重启的接管是幂等的：`new-session -A` 不复制会话；WS 重连重新鉴权；JSONL 游标在进程内重建（重启后从 0 重扫一次或由 Dashboard `refresh:full` 重建，§3.2）。

> 注意：进程重启会丢失内存态游标，重启后首轮可能**重发**部分已发过的转写行。Dashboard 端按 `uuid` 幂等去重，或重启后等一条 `refresh:full` 重建——二选一。**前提**：去重依赖 `uuid`，而 `uuid` **只在消息行**存在（§3.2 实测，辅助行无）→ 转写通道**只转发带 uuid 的消息行**，否则辅助行无法去重、每次重启重复。

### 12.5 存活探测（可选，M5）

systemd `Type=notify` + `WatchdogSec=`：控制器用 `sd_notify`（纯 Rust `sd-notify` crate）发 `READY=1` 与周期 `WATCHDOG=1`，事件循环僵死超时未喂狗即被重启。⚠️ **注意盲区（评审）**：§2 故意把 PTY 读/写放在 `spawn_blocking`/专用线程,**正是为了不让阻塞 I/O 拖住 tokio 运行时**;若由 tokio 定时器喂狗,则 PTY 读线程**永久阻塞时运行时仍健康、狗照喂、systemd 不触发**——恰恰测不到 §12.5 自称要测的"PTY 读阻塞"。要真测到,须让**每个关键阻塞线程各维护一个心跳(AtomicU64 时间戳)**,喂狗任务仅在所有线程近期有进展时才喂;否则把本节范围降为"仅测运行时级僵死"。非 systemd 环境退化为无 watchdog。

### 12.6 落盘物（实现阶段，本次仅设计）

实现时在仓库提供：
- `deploy/claude-pty-controller.service` — systemd unit（`Type=simple`/可选 `notify`，`Restart=always`，`EnvironmentFile`，非 root `User=`，可选沙箱项 `ProtectSystem`/`NoNewPrivileges`）。
- `deploy/claude-pty-controller.env.example` — `CONTROL_TOKEN` / `REMOTE_URL` / `ANTHROPIC_*`，权限 `600`。
- README 增「常驻部署」小节，指向上面两者。

## 13. 系统拓扑：三端模型（被控端 / 中转端 / 控制端）

§1–§12 描述的是**被控端**（controller，跑在 claude 机器上）。完整系统是三端：

| 端 | 角色 | 部署 | 连接方向 |
|----|------|------|----------|
| **被控端 Controller** | 本文主体：起 tmux+claude、采三通道、注入输入 | claude 所在机器（常在 NAT/防火墙后） | **主动外连**中转端 |
| **中转端 Relay** | 不透明帧路由 / 房间撮合 / 背压缓冲 / 多控制端扇出 | 公网可达的小服务 | 监听，接受两端外连 |
| **控制端 Dashboard** | 操作者界面（浏览器 xterm.js / CLI / 移动端） | 操作者处 | **主动外连**中转端 |

```
[控制端 Dashboard] ──wss/TLS──┐                         ┌──wss/TLS── [被控端 Controller]
                              ▼                         ▼
                          [中转端 Relay]   room 撮合 + 转发不透明密文帧
                              ▲                         ▲
       └──────────── E2EE (Noise) 端到端隧道，对 Relay 零知识 ────────────┘
```

- **双向外连、NAT 友好**：两端都**主动拨出**到 relay，被控机**无需开入站端口**，天然穿透 NAT/防火墙。
- **房间撮合**：relay 按 `room id`（rendezvous id，§14）把同一对 controller/dashboard 配进一个房间互转。一房间 1 controller + N dashboard。
- ⚠️ **扇出不是"转发同一密文帧"（评审 BLOCKER）**：E2EE 是**逐 dashboard 成对**的 Noise 会话（各自密钥），一份密文**无法被 N 个 dashboard 解密**。所以广播帧（通道一/二/三）必须在**被控端按每个 dashboard 各加密一次**（见 §14 扇出加密阶段），relay 只是把 per-dashboard 密文分发到对应连接——**不是 relay 复制一份不透明帧给所有人**。这与 §2 的"单 sink"只在**每连接**层面成立、不在全局成立。
- **多控制端 / 谁能驱动**：N 个 dashboard 可同时**观看**；**驱动（写入）单写**。⚠️ 此"driver 锁"是被控端**进程内**、按 dashboard 的**静态公钥身份**（§14）授权的票据，**与 §12.4 的 flock 是两套机制**（后者只防第二个控制器进程启动，不仲裁 dashboard）。需定义 acquire / 让渡 / 超时 / 断开清理;非 driver 的 `input` 帧静默丢弃 + 回事件。本地 `tmux attach` 的击键绕过此锁（§2.1），严格单写须给本地 attach 用 `attach -r`。
- **透明**：relay 只搬运**不透明密文帧**，不解析协议；透明与加密**互为因果**。⚠️ 但"只看密文"≠"房间抗滥用"：`room id` 由知道 `rendezvous_secret` 者可算出（§14），**被吊销/曾配对的 dashboard 仍能算出 room id**并占 slot / 刷连接 / DoS。故经 relay 时 **`RELAY_TOKEN` 应为必需**（非可选）+ 每 IP/每 token 连接与速率限制 + 明确"谁是权威 controller slot"（凭绑定控制器密钥的证明,而非仅 room id）。
- ⚠️ **relay 非"无状态可水平扩展"（评审）**：房间撮合要求 controller 与其 N dashboard **落到同一 relay 实例**并共享 room→连接映射 —— 这是**每实例软状态**。多实例需 **按 room id 粘性路由** 或共享撮合注册表（如 Redis）；否则两端撞不上。relay 重启丢全部 room → 两端须检测断连并重新 rendezvous。
- ⚠️ **relay 背压不能复用 §7（评审）**：§7 是**被控端本地**策略且依赖**读帧类型**做"丢旧"，而 relay 只见密文、读不出通道类型。relay 须用**每连接有界队列**：单个慢 dashboard 被丢/断,**不得 head-of-line 阻塞**整个房间;通道感知的丢旧只能在被控端的 per-dashboard 加密阶段做。
- **直连特例**：若被控端公网可达，dashboard 可跳过 relay 直连其 `wss`。⚠️ 直连的鉴权模型须与 §14 对齐：要么直连也跑 `Noise_IK` + 授权表校验（保留每设备吊销/FS），要么明示直连退化为 §5 的共享 `CONTROL_TOKEN`（**失去**每设备吊销与前向保密）—— 二者不可含糊。

## 14. 传输与端到端加密（中间流量对 Relay 不可见）

**威胁前提**：中转端**不可信**（可能是第三方 / 公有服务），它转发的终端流量含代码、密钥、对话 —— 必须让**中间流量对 relay 不可读**，同时保持透明。用**两层加密**：

**第 1 层 · 逐跳 TLS（wss / rustls）**：dashboard↔relay、controller↔relay 每条腿都 wss，防网络窃听、认证 relay 端点。但 relay **终止 TLS**、对它即明文 —— 故需第 2 层。

**第 2 层 · 端到端加密（E2EE，controller↔dashboard，Relay 零知识）** —— "中间流量加密"的核心，relay 只转发密文、**无法解密**。**v1 即采用静态密钥 + 每设备身份 + 可吊销**（`snow` 纯 Rust 提供 X25519 与各 Noise 模式）：

- **身份密钥（v1 基线）**：被控端持一对**长期静态密钥**（X25519，Noise 身份），其公钥 = 服务器身份；**每个控制端设备各持自己的静态密钥对**。被控端维护一张**授权设备表** `authorized_devices`（`{label, pubkey, added_at}` 列表）。
- **首次入网（enrollment，一次性）**：新设备用 `PAIRING_SECRET` 作 PSK 跑一次 `Noise_XXpsk3` 握手（经 relay 转发）—— 双方**交换并 pin 对方静态公钥**，被控端把新设备公钥写入授权表（带 label）。`PAIRING_SECRET` **仅用于入网**、可一次性 / 限时 / 入网后轮换，**不参与日常会话**。
- 🔴 **`PAIRING_SECRET` 必须高熵（评审 BLOCKER）**：`Noise+PSK` **不是 PAKE**——XXpsk3 把 PSK 当对称密钥混入,**主动型 relay**（本设计明确假定 relay 敌对且在握手路径上）可拿握手记录对**弱/手输** PSK 做**离线字典攻击**,猜中后冒充被控端入网一个流氓设备 = E2EE 被绕过。故 **`PAIRING_SECRET` 须 ≥128 位高熵、由扫码/复制传递,禁止人手短码**（与 §9 的 `openssl rand -hex 32` 同级）。若必须支持人手短码,改用真正的 **PAKE（CPace/SPAKE2,需额外依赖,snow 不提供）** 把离线猜解降级为每次一猜的在线猜解。
- **稳态连接（每次连接）**：用已 pin 的静态密钥跑 `Noise_IK`（dashboard 知被控端公钥、作发起方；被控端在握手内收到 dashboard 静态公钥）—— 被控端**校验该公钥 ∈ 授权表**，否则拒绝握手。产出**每会话对称密钥 + 前向保密**（临时 DH），日常连接**不再需要 `PAIRING_SECRET`**。重连即重握手换新 FS 密钥。
- **吊销（v1）**：从授权表删除某设备公钥即吊销 → 其后续握手一律失败；被控端在授权表变更时**主动断开**该公钥的在用会话（授权表热加载：信号 / 文件监视）。前向保密保证其**历史流量仍不可解**。⚠️ **非瞬时（评审）**：授权只在**握手时**校验,吊销前已建的 `Noise_IK` 会话会跑到异步断开生效为止,且 out_tx 里在途帧仍可能送达——"即刻失联"是目标、非保证。要真瞬时须**每帧/每 epoch 重查授权**;否则文档须承认这个最终一致窗口。
- **扇出加密（成对，评审 BLOCKER）**：广播输出（通道一/二/三）在被控端**按每个已连 dashboard 的 Noise 会话各加密一次** → out_rx 后接一个 dispatcher，把每条明文帧克隆给 per-dashboard 加密任务（各持一份 Noise transport + 一个 sink）。**N 个 dashboard = N 次 AEAD + N 路 sink**（CPU/带宽随 N 线性,§7 背压须按此计）。这也意味着**吊销/前向保密只在成对模型下良定义**;不要引入共享群密钥（否则吊销须全员 rekey）。
- **数据帧**：每条规范 JSON 消息（§16.3） → 序列化 → Noise transport（AEAD = ChaCha20-Poly1305）加密 → 定长前缀分帧 → **二进制不透明帧**交 relay；控制端解密还原。AEAD 自带完整性 / 抗篡改；Noise 计数器抗**同会话内**重放 / 乱序。⚠️ **跨重连重放（评审）**：重连即重握手、nonce 计数器归零,relay 可在会话边界重放旧密文,或在传输层重放/withhold 帧。`uuid` 去重只护**通道二输出**;**入站 `input` 帧无 uuid**,被重放会**重复执行命令** → 入站帧须带**应用层单调序号 + 会话绑定 nonce**,跨重连拒重复。
- **房间寻址不泄密**：`room id = HMAC(rendezvous_secret, "rendezvous" || epoch)` 截断。⚠️ **须钉死(评审)**：(1) `rendezvous_secret` 是**入网时分发的专用高熵秘密**,**不是**被控端静态**公钥**（公钥非秘密 → relay 能重算 room id 长期关联/追踪；且若源自入网密钥则任一曾配对设备可算未来所有 room id）;其生命周期**与 `PAIRING_SECRET` 解耦**,后者轮换不得改变稳态 room id。(2) `epoch` 须定义粒度与时钟模型,例 `floor(unixtime/W)`（W 为窗口）,两端各**监听/试 {epoch-1, epoch, epoch+1}** 容时钟偏移,否则轮换边界会撞空房间静默失败。(3) epoch 轮换**只影响新 rendezvous,不拆已建会话**。
- ⚠️ **身份隐藏/KCI 限定（评审）**：`Noise_IK` 对**被动** relay 隐藏静态公钥,但对**主动** relay 仅弱响应方身份隐藏（可探测/确认猜测的被控端公钥）;且 IK 有 **KCI**——被控端静态私钥泄露 → 可冒充**任意** dashboard 向被控端**注入输入(= RCE,§5)**。故被控端私钥须 OS keystore / `mlock` + `0600`,并给轮换指引。「relay 看不到任何静态公钥」仅对被动观测成立。
- **Relay 看得到 / 看不到**：看得到 = `room id`、帧大小、时序、两端 IP；看不到 = 任何明文内容、**任何静态公钥**（入网时公钥也在加密握手内传输）。元数据加固（可选）：长度分桶填充、心跳整形；默认不做、文档标注此泄露面。
- **Relay 鉴权（防滥用，正交于 E2EE）**：可选 `RELAY_TOKEN` 限制谁能用中转服务（DoS / 配额）。即便冒充者混进房间，静态公钥不在授权表 / 无入网 PSK，也过不了握手、读不到任何东西。

**密钥与令牌职责**（更新 §5）：
| 凭据 | 作用 | 谁持有 | Relay 可见 | 生命周期 |
|------|------|--------|-----------|----------|
| 被控端静态密钥对 | 服务器身份，dashboard pin 其公钥 | controller | 否 | 长期 |
| 控制端设备静态密钥对 | **每设备身份**，授权 / 吊销的单位 | 各 dashboard 设备 | 否 | 长期、**可吊销** |
| `PAIRING_SECRET` | **仅一次性入网** PSK（`XXpsk3`） | 入网时双方 | 否 | 一次性 / 限时、可轮换 |
| `RELAY_TOKEN`（可选） | 接入 relay 服务鉴权 | endpoints + relay | 是 | 长期 |

被控端静态私钥与授权表落 `${CLAUDE_PTY_HOME:-~/.claude-pty-controller}/`（权限 `600`）；dashboard 私钥存于其本地安全存储。

## 15. Windows 支持

**绝大部分已跨平台**（纯 Rust 依赖 + 文件 / CLI 接口）；唯一与 Unix 强耦合的是 tmux 承担的"会话宿主"角色。

**已跨平台、零改动**：
- **中转端、E2EE、wss、令牌与吊销**：rustls / snow / tungstenite 全跨平台。
- **通道二**：JSONL tail + 会话切换识别 + **后台 agents 多会话发现**（`claude agents --json`、`%USERPROFILE%\.claude\` 下的 `sessions\*.json` / `daemon\roster.json`）—— 纯文件 / CLI，天然跨平台（§3.2 / §3.5）。"一终端多 session" 在 Windows 上**不依赖 tmux**（后台 agent 本就无头）。
- **通道三**：OSC 状态机是字节级的（§3.3），可移植。
- 输入注入、WebSocket、背压、单实例锁（Windows 用具名互斥量 / 文件锁）。
- **PTY**：`portable-pty` 在 Windows 走 **ConPTY**（Win10 1809+；实测 0.8.1 的 ConPTY 支持 `resize()` 与 `try_clone_reader()`）。⚠️ **交叉编译不是一行**（评审）：rustls 的活动后端 `ring` 要编译 C/汇编,`x86_64-pc-windows-msvc` 目标还需 **MSVC SDK + 链接器**,从 Linux 不能裸交叉——须在 Windows/CI 上构建,或改用 `*-windows-gnu`(MinGW) / xwin 拉 SDK。

**唯一缺口 = tmux 的"会话宿主"职责**（持久留存 + 本地双向 attach + 多客户端镜像，§2.1）。Windows 无 tmux，两条路线：

- **路线 A · WSL2（推荐，v1 即可用）**：在 WSL2 里跑被控端 → **完整 Unix/tmux 模型原样适用、零改动**，覆盖多数 Windows 开发者。v1 的 Windows 支持 = "在 WSL2 中运行"。
- **路线 B · 原生 ConPTY + 自建会话宿主（v2/v3）**：把 tmux 那层抽象成 `SessionHost` trait，双实现：
  - `TmuxHost`（Unix / WSL）= 现状。
  - `ConPtyHost`（原生 Windows）= 一个**独立长存的会话宿主进程**（Windows 服务 / 分离进程）持有 ConPTY + claude；控制器经**命名管道** attach。关键：ConPTY 由创建进程拥有、进程退则 claude 亡 —— 故**持久留存必须靠这个独立宿主进程**，控制器重启后经命名管道重接管。⚠️ **"自实现 tmux 多客户端"被严重低估(评审)**：这等于做一个 mini-tmux —— 要解决 ① 一路 ConPTY 输出扇出给 N 个管道客户端(独立游标+背压)、② 多客户端**输入仲裁**(无 tmux 的 `attach -r` 可借)、③ **resize 仲裁**(ConPTY 单几何,同 §2.1 的"谁赢")、④ 崩溃后重 attach 握手与客户端存活检测、⑤ host↔控制器线协议。§2.1 为 tmux 版写了 ~25 行,这条更难却只两句。明确列为 v2/v3 非平凡工作项。

**平台差异表**：

| 维度 | Unix（tmux） | 原生 Windows |
|------|-------------|-------------|
| 会话宿主 / 持久 | tmux `new-session -A` | 独立 `ConPtyHost` 进程 + 命名管道 attach |
| 本地双向 | `tmux attach` 多客户端 | 第二个命名管道客户端 |
| resize | `master.resize()` | ConPTY resize（同 API） |
| 标题信号 | claude 发 **OSC 0**（`SET_TITLE_AND_ICON`，实测） | **待 Windows 构建复核**：claude 在 Linux 二进制里始终用 OSC 0（无 `SetConsoleTitle`/`kernel32`）;`process.title` 只设**进程名**、非控制台标题,二者不可等同。若 Windows 经典控制台/ConPTY 吞掉 OSC 0 则标题信号失效 → 退回**按行 `sessionId`**（不影响识别） |
| 屏幕 / tab_status | OSC 21337（tmux DCS 包裹）；status off 消噪 | OSC 21337 裸发,但 ⚠️ **ConPTY 重绘会刷屏(评审)**:ConPTY 整屏重 paint 会**重复/灌满通道一**(无 tmux `status off` 类开关),且实测 claude 自身在 "Windows over SSH (ConPTY re-rendering)" 下**主动关全屏**。需 vt100 屏幕 diff 去重 / `CLAUDE_CODE_NO_FLICKER=1`。**不比 tmux"更简单"** |
| 信号 | SIGHUP/TERM/WINCH | 控制台控制事件 / Job Object；`ctrlc` |
| 常驻（§12） | systemd | **Windows 服务**（或任务计划 / NSSM），自愈链同构 |
| 配置路径 | `~/.claude` | `%USERPROFILE%\.claude`（`dirs` crate 处理） |

> 结论：Windows 上**通道二 / 三、多会话、E2EE、relay 全部 v1 即可用**；唯一需平台分叉的是"会话宿主"—— v1 用 **WSL2** 直接拿到完整能力，原生 `ConPtyHost` 列 v2/v3。标题信号差异已由按行 `sessionId` 兜底。（Windows 具体行为应在 Windows 构建上复核，见顶部「版本敏感性」。）

## 16. 适配器层：兼容其他 agent TUI

核心（PTY / 会话宿主、WebSocket、relay、E2EE、背压、常驻）**与具体 agent 无关**；只有"三通道里 agent 专属的部分"需要适配。把它收进一个 `AgentAdapter` 抽象 —— **新工具只要写个 adapter 就能支持**。Claude 的 §3.2/§3.3/§3.5 即第一个实现 `ClaudeAdapter`。

### 16.1 分层：哪些通用、哪些要 adapt

| 通道 | agent 专属？ | 说明 |
|------|:---:|------|
| **通道一 · 屏幕** | **否（零-adapter 基线）** | 任何 TUI 都跑在 PTY 里 → 屏幕镜像**对所有 agent 天然可用**。没 adapter 也能远程看 / 驱（§3.4 的"模式无关"推广到"agent 无关"）。 |
| **通道二 · 转写** | **是** | 对话持久化的位置与格式各家不同（Claude=JSONL；别家=md / sqlite / 无）。adapter 负责**定位源 + 解析成规范事件**。 |
| **通道三 · 状态** | **是** | 状态信号机制各异（Claude=OSC 21337+bell；别家=别的 OSC / 标记 / 无 → 从屏幕或转写推断）。adapter 负责**产出规范状态**。 |

> 关键：**通道一是零-adapter 通用底线，通道二/三按 adapter 渐进增强**。全新工具即便没 adapter，也能当纯屏幕镜像远程用；写了 adapter 才有结构化对话与状态。支持成本**单调递增**：先白嫖通道一，再按需补二/三。

### 16.2 `AgentAdapter` 抽象（设计级）

```rust
trait AgentAdapter {
    fn id(&self) -> &str;                        // "claude" | "aider" | …
    fn capabilities(&self) -> Capabilities;      // {transcript, status, multi_session, launch}

    fn launch_spec(&self, cfg: &Cfg) -> LaunchSpec;       // 命令/参数/env（交宿主 tmux/ConPTY 运行）
    fn discover(&self, ctx: &Ctx) -> Vec<SessionRef>;     // 会话发现（多会话；单会话返回一个），见 16.4

    // 通道二
    fn transcript_sources(&self, s: &SessionRef) -> Vec<Source>;                  // 要 tail 的文件/流
    fn parse_transcript(&self, raw: &[u8], src: &Source) -> Vec<TranscriptEvent>; // → 规范

    // 通道三
    fn status_strategy(&self) -> StatusStrategy;          // Osc | TranscriptDerived | Registry | ScreenHeuristic | None
    fn parse_status(&self, input: StatusInput) -> Option<StatusEvent>;            // → 规范

    // 入站（评审 ADP-1：原缺，输入侧也是 agent 专属）—— 把规范动作映射成该 agent 的字节序列
    fn encode_input(&self, cmd: InboundCmd) -> Vec<PtyWrite>;
    // InboundCmd = Submit | Interrupt | Eof | Key(chord) | Paste(text) | Resize{cols,rows} | Refresh{scope}
    // ClaudeAdapter: Submit→"\r"、Interrupt→0x03、Eof→0x04、Paste→bracketed-paste、Resize→master.resize
}
```

核心不认识任何 agent，只调 trait。Adapter 选择：按启动命令名映射 / `--adapter <id>` 显式 / 默认 `GenericAdapter`（仅通道一 + raw 字节透传 + 可选屏幕启发式状态）。
> ⚠️ **输入侧曾是 TODO（评审 ADP-1）**：§4 的 `input`(追加 `\r`)/`raw`/`refresh` 是 Claude/键盘形状,硬编进核心则"换 agent 改核心"。故 `encode_input` 上提到 trait：`Submit`/`Interrupt`/键位/粘贴由 adapter 翻译,`Resize` 留核心(SIGWINCH 通用),`refresh` 仅在 `capabilities.transcript` 时有意义。**零-adapter 只保证"观看(屏幕镜像)";"驱动"在无输入映射时仅 raw 字节尽力而为**（ADP-8）。

### 16.3 规范化线缆 schema（Dashboard 面向它 → 换 adapter 前端零改）

```jsonc
// 通道一（通用，多会话加 session 标签）
{"type":"output","session":"<id>","raw":"…"}

// 握手（评审 ADP-4：连接时 + agent/会话变更时发，让前端预知能力，区分"无转写"与"暂时静默"）
{"type":"hello","v":1,"agent":"claude","capabilities":{"transcript":true,"status":true,"multi_session":true,"input":true}}

// 通道二：规范转写事件（+ raw 逃生舱保真）
{"type":"transcript","v":1,"agent":"claude","session":"<id>","ts":169..,
 "role":"user|assistant|tool|system",
 "parts":[ {"kind":"text","text":"…"}
         | {"kind":"thinking","text":"…"}                      // 评审 ADP-2：实测有 thinking 块,须建模否则丢
         | {"kind":"tool_use","id":"…","name":"Bash","input":{…}}
         | {"kind":"tool_result","forId":"…","content":"… | [blocks]"} ],  // ADP-3：content 可为字符串或块数组(含图片)
 "msgUuid":"…","partIndex":0,        // 评审 ADP-7：一条 JSONL 行可展开成多事件 → 去重 key = (msgUuid, partIndex),非裸 uuid
 "raw":{…}}                          // agent 原生记录;含 thinking/图片/结构化内容时 raw 不可省

// 通道三：规范状态（稳态）+ 通知（边沿）—— 评审 ADP-6：notify 是一次性边沿,不是状态,拆开
{"type":"event","v":1,"agent":"claude","session":"<id>",
 "state":"idle|working|waiting",          // 稳态,三选一
 "detail":{…},"raw":{…}}
{"type":"notify","v":1,"agent":"claude","session":"<id>","raw":{…}}   // 边沿,尽力而为(bell),不可当权威回合结束
```

**状态映射（ClaudeAdapter，实测）**：claude OSC `tab_status` 恰为 `Idle/Working…/Waiting`（§3.3，二进制核对无 approval）→ 规范 `idle/working/waiting`。**不臆造 `awaiting_approval`**。`notify` 拆成独立边沿消息（仅 bell 时、尽力而为）——**回合结束的权威信号仍是 `tab_status` 跃迁**,前端勿把 `notify` 当回合界。其它 adapter 有真信号可扩 enum,但须在其文档注明来源；规范 enum 只承诺各 adapter 能落地的子集。
> **schema 版本策略（评审 ADP-5）**：加法变更（新 `kind`/新 `state`/新可选字段）保持 `v:1`,前端**忽略未知**;仅破坏性变更升 `v`,并由 `hello` 播报控制器支持的最高 `v` 供前端降级告警。§1 所谓"协商"即此 `hello` 播报 + 忽略未知,非双向谈判。

> 相对 §3 的 Claude 原生载荷的演进 = **规范字段 + `raw` 逃生舱**。ClaudeAdapter 既填规范 `parts`/`state`、也带 `raw`（Claude JSONL 行 / 原始 OSC），保真不丢；前端按规范 schema 写，任何 adapter 通用。**入站同理**：§4 的 `input/raw/resize/refresh` 也属 adapter 面（输入映射），新 agent 若键位 / 命令不同需在 adapter 内翻译（§16 待补输入侧 trait 方法）。

### 16.4 会话发现策略（枚举）

- `Cli { cmd, parse }`：跑命令拿 JSON（Claude = `claude agents --json`，§3.5）。
- `FsWatch { glob, identity }`：监视文件、按某字段认会话（Claude = `projects/**/<sid>.jsonl` + 行内 `sessionId`，§3.2.1）。
- `Single`：就一个前台会话。
- `None`：不做结构化发现，退到通道一。

### 16.5 Claude = 首个 adapter（把已有设计归位）

`ClaudeAdapter`：`launch`=`claude`；`discover`=`Cli(claude agents --json)` ∪ `FsWatch(projects 树)`；`transcript_sources`=`<sid>.jsonl`（+ v2 子代理 sidechain）；`parse_transcript`=Claude JSONL→规范 parts；`status_strategy`=前台 `Osc`(21337/bell，§3.3) + 后台 `Registry`(sessions/*.json `status`，§3.5)。即 §3.2–§3.5 全部归入此 adapter。

### 16.6 适配一个新工具要回答的四问（示例，均须按该工具实测核对）

以假想 `foo` TUI 为例，写 `FooAdapter` 即回答：
1. **怎么启动** → `launch_spec`。
2. **对话写哪、什么格式** → `transcript_sources` + `parse_transcript`（把 foo 的历史文件 / SQLite / stdout 结构化成规范 parts）。**查不到就声明 `capabilities.transcript=false`**，退化到通道一。
3. **状态怎么知道** → `status_strategy`：有 OSC 就 `Osc`；否则从转写推断（`TranscriptDerived`，如"出现 assistant 新消息=working→完成=idle"）；再否则屏幕启发式或 `None`。
4. **多会话吗** → `discover` 策略；没有就 `Single`。

> **优雅降级是一等公民**：adapter 按 `capabilities` 声明能力，核心据此只发能发的通道。最坏（无 adapter / 全 false）= 纯通道一镜像，仍可远程操作。各工具的转写位置 / 状态机制属**外部易变细节**，按目标工具版本实测（同顶部「版本敏感性」），adapter 把差异**隔离在一个文件里**，核心不动。

> 命名注记：仓库名 `claude-pty-controller` 是历史名；架构上 Claude 只是首个 adapter，工具本身是"agent-TUI 远程控制器"。

**中转端实现**：无业务逻辑的**独立小程序** —— 建议同 Cargo workspace 第二 binary `relay`（或独立部署）。职责仅：endpoint 鉴权（`RELAY_TOKEN`，经 relay 时**必需**，§13）、room 撮合、不透明帧双向转发、**每连接**有界队列 / 背压（§13——relay 只见密文,不能复用 §7 的通道感知丢旧）、断连重连、把被控端的 **per-dashboard 密文**分发到对应连接（**非**复制同一帧给所有人,§14 扇出加密）。**有每实例 room 软状态**：多实例须按 room id 粘性路由或共享撮合注册表（§13），非真无状态。被控端 `REMOTE_URL` 指向它。

## 17. 已知缺口与评审待办（落地前须决议）

本文经两轮独立多 agent 评审（对 claude v2.1.163 + tmux 3.4 + Cargo.lock 实测）。下列横切项已拆成 **GitHub issues** 跟踪（[全部 from-review](https://github.com/sanchuanhehe/claude-pty-controller/issues?q=is%3Aissue+label%3Afrom-review)）：

- 🔴 **BLOCKER**（[#1](https://github.com/sanchuanhehe/claude-pty-controller/issues/1)）入网抗主动 relay —— `PAIRING_SECRET` 强制高熵 vs 引入 PAKE（§14）。
- 🔴 **BLOCKER**（[#2](https://github.com/sanchuanhehe/claude-pty-controller/issues/2)）成对 E2EE 的 per-dashboard 扇出加密阶段须真正进 §2 进程模型与 §7 背压（§13/§14）。
- **控制端 Dashboard 契约**（[#3](https://github.com/sanchuanhehe/claude-pty-controller/issues/3)）：规范 schema（§16.3）、`hello` 能力协商、重连/快照/去重。
- **机密红action 决策**（[#4](https://github.com/sanchuanhehe/claude-pty-controller/issues/4)）：API key/密钥经通道一/二 E2EE 送达已授权 dashboard，是否有意 + 可选脱敏。
- **协议版本**（[#5](https://github.com/sanchuanhehe/claude-pty-controller/issues/5)）：`hello` 播报 max `v` + 加法保 `v:1`/忽略未知（§16.3 ADP-5）+ 破坏性升级流程。
- **运维面**（[#6](https://github.com/sanchuanhehe/claude-pty-controller/issues/6)）：配置 schema、结构化日志事件、错误分类、测试（OSC/JSONL 单元+模糊）、可观测、限流、审计（§12 OPS-8）。
- **录制/回放 + vt100 快照**（[#7](https://github.com/sanchuanhehe/claude-pty-controller/issues/7)，v2，§7）。
- **依赖未验证**（[#8](https://github.com/sanchuanhehe/claude-pty-controller/issues/8)）：`snow`/PAKE/`flock`/`sd-notify` 未入 `Cargo.toml`，M4/M5 前补齐 + `cargo check`（§6/§12）。
- **原生 Windows `ConPtyHost`**（[#9](https://github.com/sanchuanhehe/claude-pty-controller/issues/9)，v2/v3）：mini-tmux 级工作量 + msvc 工具链（§15）。

> 详细发现（含每条实测命令与证据）见各 §的 ⚠️ 评审注、上述 issues 与 git 历史的两轮评审 commit。
