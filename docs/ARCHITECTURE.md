# claude-pty-controller — 架构设计

> 把一台运行 Claude Code 的机器，通过单一静态 Rust 二进制暴露为可远程观测 / 驱动的终端会话。
> 本文是在初版方案 `方案6-Rust版.md`（源文件已删除,保留在 git 历史中）基础上、修正其若干缺陷后的落地设计；与原方案的关键差异及原因汇总见 §10。

> ⚠️ **版本敏感性**：本文对 Claude Code 内部细节的论断，关键面已**对环境实际安装的 claude v2.1.163 实测核对**（`claude agents --json`、`~/.claude/sessions/*.json`、`daemon/roster.json`、`projects/<sanitize(cwd)>/<sessionId>.jsonl`、二进制里 OSC `21337` 与 `Ptmux;` 仍在、status 文本非 "generating"，见 §3.5）。其余细节核对自一个**可能偏旧**的源码快照。设计有意**架在跨版本稳定的不变量上**（PTY 字节流、行内 `sessionId`/`cwd` 戳、tmux 自省、文件系统观测），易变细节降级为「实现时按**目标 claude 版本**现场复核」—— 变了只需调参数、不动架构。

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
- 多会话结构化 fan-out（v1 通道一即透明覆盖单前台会话；后台 agents 的并发 tail + 状态列表为 v2，已实测可行，见 §3.5）。
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

#### 3.2.1 运行时会话切换识别（/resume、新开、切换）

用户可能在 pane 里 `/resume`、`claude --resume`、或退出后重开 claude —— 此时**活跃 JSONL 会变**：核对源码，resume 走 `switchSession(newSid, projectDir)` 后**追加写到被恢复会话已有的 `<uuid>.jsonl`**，且 `projectDir` 可能**指向另一个项目目录**。启动时锁定的单文件就此失效，必须运行时重新识别。

可用信号按可靠性排序：

1. **每行的 `sessionId`（唯一真值，主信号）**：源码 `sessionStorage.ts` 给每条转写行都盖 `sessionId` + `cwd` 戳（`SerializedMessage`，写入时强制覆盖）。读到的行 `sessionId` **一旦与当前追踪值不同 = 切换**。与文件名/路径无关，最可靠。
2. **活跃文件 = 当前正在增长的 `*.jsonl`（文件发现）**：单个受控 pane 同一时刻只有一个会话在写，"正在增长的那个"无歧义。每个 poll 复评，非锁定文件开始增长即切换目标。
3. **OSC 0 标题变化（次要佐证）**：切换会重发 `ESC]0;<title>`（§3.3），可作为"可能切换"的提示去主动复扫；但标题为空时不发，不能单独依赖。
4. ~~OSC 7 cwd~~：源码**不发** OSC 7，cwd 只在 JSONL 行里 —— 拿不到终端信号。

**识别 + 重锁流程**：
```
每个 poll / 收到标题变化提示:
    scope = 当前 cwd 的项目目录（若锁定文件转静默且 scope 内无增长 → 回退扫全 projects/ 树）
    active = scope 内最近在增长的 *.jsonl（排除 subagents/、*.meta.json、timeline、bridge-pointer）
    读 active 行的 sessionId:
        == 当前 sid 且 == 锁定文件 → 正常 tail（§3.2）
        != → 会话切换：
              current_sid = 新 sid；锁定 active；last_offset = 0
              发 {"type":"session", sessionId, cwd, path, reason:"resume|new|switch"}
              从头 tail 新文件
```

**多 claude 实例消歧**：同机若有别的 pane 在写 jsonl，"全局最近增长"可能误指他人会话。用**本 pane 的 `tab_status` 活动窗口**做归属门槛 —— 仅当某文件**在本 pane 处于 `Working…` 期间**增长、或其行 `sessionId` 与既有追踪一致时，才采纳为本会话。把通道三的活动状态耦合进通道二的发现，避免抢错。

**取舍**：切换时 `last_offset=0` 会**重发被恢复会话的全部历史**（resume 文件本含历史）—— 对前端是"完整呈现恢复后的对话"，但大历史有一次性灌洪，靠有界背压（§7）+ 前端按 `uuid` 去重消化。新增出站控制消息 `{"type":"session",…}` 让前端在切换点清空/重建对话视图。

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

## 3.4 模式无关性与优雅降级（含 `claude agents` / 全屏 UI）

claude 在 pane 里会进入各种非常规对话状态：`/resume` 选择器、plan 模式、`/agents`（REPL 内配置 UI）、以及 **`claude agents`**（**实测 v2.1.163**：打开一个**交互式 agent view**，派发/监控 daemon 托管的后台 agent 会话 —— 不是旧源码的"列出即退"；多会话细节见 §3.5）。设计原则：

- **通道一（屏幕）模式无关、永远可用**：tmux 镜像的是 pane 的最终 ANSI 画面 —— 无论 claude 渲染的是对话、全屏 alt-buffer UI、还是列表输出，远程 Dashboard + 本地 attach 都**透明可见可驱动**。`claude agents` / `/agents` 这类模式**仅靠通道一即可完整远程操作**。
- **通道二/三 best-effort、优雅降级**：这些模式不产生转写、不发状态 OSC 时，通道二自然**安静**、通道三仅余 idle 跃迁 —— 这是**预期降级，不是故障**：没转写就不发 transcript，没 OSC 就不发 event，通道一照常。控制器不得因通道二/三无数据而误判会话异常。
- **回到对话由 §3.2.1 兜底**：从 `/agents` 选中并运行某 agent → 进入正常会话（同一 `<sessionId>.jsonl`，通道二恢复）或派生子代理；会话切换/恢复一律由按行 `sessionId` 重锁覆盖。

> **子代理（Task fan-out）**：其内部对话写在独立 sidechain `…/<sessionId>/subagents/agent-<agentId>.jsonl`，主转写只含 `Task` 的 tool_use + 最终 tool_result（即"看得到 agent 的输入与最终产出，看不到内部步骤"）。v1 通道二**仅 tail 主文件**；**多文件嵌套 tail 子代理对话**列为 v2（§11）—— 届时并发 tail 各 sidechain（每文件独立换行游标），按 `agentId` / `isSidechain` / `parentUuid` 还原嵌套树，并按主转写里 `Task` tool_use.id ↔ `agent-<id>` 关联归属。远程 agents（CCR）只有 `remote-agents/*.meta.json`，转写在云端、本地不可 tail。

## 3.5 多会话 / 后台 agents（一终端管多 session）—— 实测 v2.1.163

已对**环境中实际安装的 claude v2.1.163** 验证：所谓"一终端管多 session"是 `claude agents` 打开的 **agent view**，它**派发并监控由 daemon 托管的后台 agent 会话**——**不是**旧源码里的 tmux 分屏 / teams（本机 `~/.claude/teams/` 为空）。后台 agent 是**无前台终端的 daemon worker 进程**，各有独立 `<sessionId>.jsonl` 与状态。

**实测可用的发现面（按优先级，均无需解析合流字节）**：

1. **`~/.claude/sessions/<pid>.json`（首选，直接读 / inotify）** —— 每个活跃会话一份：
   `{pid, sessionId, cwd, kind:"interactive"|"bg", status:"busy"|…, name?, jobId?, updatedAt, entrypoint, version, bridgeSessionId}`。读目录即得全部会话 + 状态 + 心跳，零子进程。
2. **`claude agents --json`（官方脚本接口）** —— 免 TTY 打印 live sessions 数组并退出：`{pid, cwd, kind, sessionId, status, name?}`。语义同上，但每次起子进程、较重；作可移植官方 API / 交叉校验。
3. **`~/.claude/daemon/roster.json`（后台细节）** —— daemon 托管 worker：每个含 `sessionId, cwd, jobId, pid` 及 **`ptySock`**（`/tmp/cc-daemon-*/…/pty/<jobId>.sock`，后台会话 PTY 的 unix 套接字）、`rendezvousSock`、`dispatch`（启动方式 / `cols` / `rows`）。`daemon.status.json` 有 `supervisorPid`。
4. **文件系统兜底** —— 并发增长的 `*.jsonl` + 行内 `sessionId`（§3.2.1 的版本无关地板）；当上面格式变动时退化使用。

**会话 → 转写路径（实测确认）**：`~/.claude/projects/<sanitize(cwd)>/<sessionId>.jsonl`，`sanitize = [^a-zA-Z0-9]→-`（例：`/root/ws63-rs` → `-root-ws63-rs`）。

**据此分通道**（通道一 v1 已覆盖前台；多会话 fan-out 为 v2）：

- **通道二（多会话并发 tail）**：从发现面 1/2 拿到会话集合 → 各自解析转写路径 → **并发 tail**（每文件独立换行游标，§3.2）→ 每条标注 `sessionId`/`name`/`kind`。后台 agent 与前台交互会话**一视同仁**，Dashboard 可分流 / 列表展示。
- **通道三（状态，免 OSC）**：后台 agent 无前台终端，其 `status`（busy/idle）+ `updatedAt` 直接从 `sessions/<pid>.json` / `agents --json` 读，**不必解析 OSC**；前台交互会话仍走 OSC `tab_status`（§3.3）。两路都喂 `refresh_tx`（§3.2 回合结束触发）。
- **通道一（屏幕）**：前台交互会话 = 控制器的 tmux pane（如常）。**后台 agent 无前台画面**，v1 仅以"转写 + 状态"两路呈现；`claude agents` 的 agent view TUI 本身只是 pane 里一屏，通道一透明可见可驱动。
  - *v2 探索（不依赖）*：daemon 为每个后台 agent 暴露 `ptySock`，理论上控制器可连该 unix 套接字流式取 / 驱其屏。但这是 daemon 内部协议（`proto:1`、rendezvous/pty 双 socket），**版本敏感、未公开**，仅作探索项，绝不作 v1 依赖。

> 本节相比上一版（基于旧源码 swarm/tmux 的猜测）已**改为实测 daemon + `claude agents --json` + `~/.claude/sessions/*.json`** 的确定面。即便未来版本调整发现面，§3.2.1 的"按文件 + 行内 `sessionId`"仍是地板，架构不动。

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
| 会话切换识别 | 未涉及 | 追踪每行 `sessionId` + 活跃文件复评 | /resume 会换文件（甚至换项目目录），靠行内 `sessionId` 唯一真值重锁（§3.2.1） |

## 11. 里程碑

1. **M1 骨架**：PTY 跑 tmux `new-session -A` + `allow-passthrough` + 通道一（尾字节缓冲）+ ws_outbound，本地 `ws://127.0.0.1` 跑通画面。
2. **M2 双向**：入站 input/raw/resize（`master.resize`）+ 鉴权 + wss；验证本地 `tmux attach` 与远程同时透明驱动。
3. **M3 通道二/三**：jsonl 差集锁定 + tail（末换行游标）+ **会话切换识别（按行 `sessionId` 重锁，§3.2.1）** + OSC 状态机（`&[u8]` + tmux/screen 解包）+ 三源刷新（轮询 / 状态事件 / 手动）。
4. **M4 健壮性**：有界背压 + 重连退避 + 优雅 detach（保留会话）+ tmux client 退出处理 + **单实例锁**（§12）。
5. **M5（v2 / 可选）**：**后台 agents 多会话 fan-out**（发现面 `sessions/*.json` / `agents --json` + 并发 tail + 免-OSC 状态，§3.5）+ 子代理嵌套 tail（§3.4）+ vt100 重连快照 + notify + base64 模式 + 审计日志 + systemd watchdog（§12）。
6. **跨阶段校验**：对照**目标 claude 版本**复核版本敏感常量（OSC/status、JSONL 路径与字段、发现面 schema）—— 见顶部「版本敏感性」。

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

- 启动时对 `${XDG_RUNTIME_DIR:-/tmp}/claude-pty-controller-<session>.lock` 做 `flock(LOCK_EX|LOCK_NB)`；拿不到锁 → 说明已有实例 → **直接退出**（让守护不致起重复实例），或按 flag 改为「抢占：发信号让旧实例 detach 后再接管」。
- 锁的粒度 = 每个 tmux 会话名一把，未来多会话（非目标 v1）天然隔离。
- 重连/重启的接管是幂等的：`new-session -A` 不会复制会话；WS 重连重新鉴权；JSONL 游标在进程内重建（重启后从 0 重扫一次或由 Dashboard `refresh:full` 重建，§3.2）。

> 注意：进程重启会丢失内存态游标，重启后首轮可能**重发**部分已发过的转写行。Dashboard 端应按 `uuid` 幂等去重（消息本就带 `uuid`），或重启后等一条 `refresh:full` 重建——二选一，文档标注由前端去重。

### 12.5 存活探测（可选，M5）

systemd `Type=notify` + `WatchdogSec=`：控制器用 `sd_notify`（纯 Rust `sd-notify` crate，无 C 依赖）发 `READY=1` 与周期 `WATCHDOG=1`。当事件循环卡死（如 PTY 读阻塞、死锁）超过 watchdog 周期未喂狗，systemd 判定僵死并重启 —— 比单纯「进程还在」更真实的存活信号。非 systemd 环境退化为无 watchdog。

### 12.6 落盘物（实现阶段，本次仅设计）

实现时在仓库提供：
- `deploy/claude-pty-controller.service` — systemd unit（`Type=simple`/可选 `notify`，`Restart=always`，`EnvironmentFile`，非 root `User=`，可选沙箱项 `ProtectSystem`/`NoNewPrivileges`）。
- `deploy/claude-pty-controller.env.example` — `CONTROL_TOKEN` / `REMOTE_URL` / `ANTHROPIC_*`，权限 `600`。
- README 增「常驻部署」小节，指向上面两者。
