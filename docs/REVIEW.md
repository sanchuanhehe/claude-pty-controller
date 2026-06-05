# 方案 6 Rust 版 — 评审意见

对初版方案 `方案6-Rust版.md`（原稿已并入本仓库的架构与评审,源文件保留在 git 历史中）的技术评审。整体方向（单一静态二进制、三通道、复用现有 Dashboard 协议）是成立且有价值的，但稿件中的示例代码存在若干**会导致功能不可用或数据损坏的缺陷**，必须在落地前修正。下文按严重程度排列。

## 阻断级（Blocker）— 不修无法工作

### B1. WebSocket 写线程绕过了帧封装，协议被破坏
`connect_ws` 里用 `ws.get_mut().try_clone()` 克隆底层 TCP 流，然后在写子线程里 `s.write_all(msg.as_bytes())` 直接写裸字节。

- WebSocket 的帧状态（opcode、长度、**客户端必须的掩码 masking**、分片）保存在 `tungstenite::WebSocket` 结构里，不在裸 `TcpStream` 里。直接往 socket 写 UTF-8 文本，对端不会把它当成合法 WebSocket 帧 —— 连接会被服务器按协议错误关闭。
- `MaybeTlsStream` 在 WSS 下也无法 `try_clone`。
- 同一个 WebSocket 被读线程和写线程同时碰底层流，没有同步。

**结论：当前 WS 读写拆分方式从根上是错的。** 见架构文档 §WebSocket：改用 `tokio` + `tokio-tungstenite` 的 `.split()`（`SplitSink`/`SplitStream` 各自持有正确的帧状态），或在同步模型下用 `Arc<Mutex<WebSocket>>` 统一调用 `.send()`/`.read()` 而不是写裸流。

### B2. `wss://` 在当前依赖配置下根本不可用
`Cargo.toml` 写的是 `tungstenite = "0.24"`，没开任何 TLS feature，所以**只能 `ws://` 明文**。文稿又宣称"无 OpenSSL"且部署示例暗示走公网 relay。要做到「无 OpenSSL 且支持 TLS」必须显式启用 rustls feature（`rustls-tls-webpki-roots`）。当前配置与安全部署目标矛盾。

### B3. PTY 输出用 `from_utf8_lossy`，多字节字符在 8KB 读边界被破坏
`reader.read()` 每次最多 8192 字节，UTF-8 多字节序列（中文、emoji、部分 box-drawing 字符）会被切在两次读取之间。`String::from_utf8_lossy` 把半个字符替换成 U+FFFD，**数据永久损坏**，xterm.js 渲染出乱码。
**修法**：缓存尾部不完整的 UTF-8 字节，只发送完整 UTF-8（ANSI 控制字节全是 ASCII，安全）；或改用 base64 无损传输。见架构 §通道一。

### B4. Resize 用写 ANSI 序列实现，对 PTY 无效
`Incoming::Resize` 时往 PTY 写 `\x1b[8;rows;cols t`。这是**给终端模拟器看的 xterm 命令，不是给内核 PTY 的**。内核不会改 winsize，子进程（claude）收不到 `SIGWINCH`，也读不到新尺寸。正确做法是保留 `master` 句柄并调用 `master.resize(PtySize{...})`。但 `PtySession::spawn` 把 master 丢弃了（只留 writer + child），**连句柄都没保留**。

### B5. `Outgoing::Event { ..., .. }` 不是合法 Rust
§3c / §7 多处用 `Outgoing::Event { event: "...".into(), status, .. }`。`..` 的"剩余字段用默认值填充"语法**不适用于 enum 的结构体变体**（无法对枚举变体做 functional update）。编译不过。需要定义一个独立的 `Event` 消息结构体，字段全为 `Option` 并配 `#[serde(skip_serializing_if = "Option::is_none")]`。

## 严重级（Major）— 会丢数据或留隐患

### M1. JSONL 增量读取会丢失"半行"
`read_lines_from` 从 `last_size` 读到文件末尾后，把 `last_size = meta.len()`。如果 Claude Code 正写到一半（最后一行还没写完 `\n`），这次会把不完整的 JSON 读出来 → `serde_json` 解析失败被静默丢弃 → 但 `last_size` 已经推进到不完整行之后 → 下次只读到这行的尾巴，**整行永久丢失**。
**修法**：只把偏移推进到「最后一个换行符」之后，未闭合的尾部留到下次。见架构 §通道二。

### M2. 缺少鉴权与威胁模型
WebSocket 无任何 token / 鉴权，且控制器把远端消息直接注入到运行 `claude`（持有 `ANTHROPIC_API_KEY`）的 PTY。任何能连到 relay 的人都能驱动这台机器上的 shell、读写文件、花掉 API 额度。这是**远程代码执行级别**的能力，必须有共享密钥握手 + 强制 `wss://`，并在文档里写清威胁模型。

### M3. OSC 状态机用 `byte as char` 构造 `String`，按字节切片会 panic
`let ch = byte as char;` 把单字节当 Latin-1 char。字节 0x80–0xFF 进 `String` 会变成 2 字节 UTF-8；后续 `&seq[2..]`、`&self.dcs_buf[3..len-2]` 这类**按字节索引切片**遇到非字符边界会直接 panic。OSC 标题里带中文时必现。
**修法**：状态机整体在 `&[u8]` 上做，字段提取时再 `str::from_utf8`。见架构 §通道三。

### M4. tmux DCS 解包偏移错误
`let inner = &self.dcs_buf[3..self.dcs_buf.len()-2];` 只剥掉了 `ESC P` + 1 个字符和尾部 `ESC \`，但 tmux passthrough 的前缀是 `ESC P tmux ;`（`\x1bPtmux;`）。`tmux;` 没被剥掉，内层 OSC 解析必然失败。需要正确剥前缀 `\x1bPtmux;` 和后缀 `\x1b\\`，再把内部 `\x1b\x1b` 还原成 `\x1b`。

### M5. 断连即无界堆积，且新连接无法恢复当前画面
- WS 断开期间，PTY 输出和 JSONL 消息仍源源不断塞进 `mpsc`（无界 channel）→ 内存无限增长；重连后一次性灌一大坨历史。需要**有界 channel + 丢弃/合并策略**。
- 新接入的 Dashboard 只能看到「之后」的输出，看不到当前屏幕。需要维护一份屏幕快照（如 `vt100` crate 解析），连接时先发快照。可作为 v2，但要在文档里标注此限制。

## 次要级（Minor）— 健壮性 / 一致性

- **m1**：`WsHandle` 只暴露 `write_tx` / `read_rx`，但主循环调用 `ws.send()` / `ws.recv()`（不存在）。伪代码接口不自洽。
- **m2**：空环境变量也会被注入（`ANTHROPIC_BASE_URL=""` 会覆盖子进程已有值）。应跳过空值。
- **m3**：`spawn(shell) 再 pty.write("claude\r")` 在 shell 下多套了一层，shell 自身输出会混进终端流，且依赖 PATH 和提示符时机。建议直接把 `claude` 作为 `CommandBuilder` 命令启动（可配置），需要时再回退到 shell。
- **m4**：子进程退出（reader 返回 0）后主循环仍在 50ms 空转，不会自动收尾。应监测 child 退出并触发优雅关闭。
- **m5**：「最新 JSONL 用 mtime 选」是启发式，多 session 并发时会抖动；`timeline.jsonl` 用字符串包含判断也偏脆。可接受，但建议结合 session-id 锁定，并标注已知限制。
- **m6**：文稿说"四个线程"，实际 `connect_ws` 又额外起了读线程 + 写子线程，描述与实现不符。

## 评审结论

方向认可，**通道二/通道三/部署模型可以保留**。但 WebSocket 读写拆分（B1）、TLS 配置（B2）、PTY 输出编码（B3）、resize（B4）、Event 消息类型（B5）这五处必须重做，JSONL 半行（M1）和鉴权（M2）必须补。架构文档 `ARCHITECTURE.md` 已按上述修正给出落地设计。
