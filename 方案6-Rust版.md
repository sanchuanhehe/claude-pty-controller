# 方案 6 — Rust 版：PTY + JSONL + OSC + WebSocket

## 0. 一句话

把方案 6 的 Controller 从 Node.js 改为 **Rust 单一静态二进制**。编译完 `scp` 到服务器，无运行时依赖。

`claude-pty-controller` — 一个二进制文件 ≈ 3MB。`./claude-pty-controller` 回车即跑。

## 1. 架构

```
claude-pty-controller (单一进程, 四个线程)
│
├─ 线程 1 (PTY 读):
│   portable_pty::MasterPty::read() ──阻塞读──► 终端 ANSI 数据
│   │
│   ├──► 通道一: 原始 ANSI 字节 ──► Outgoing::Output { raw } ──► ws_tx
│   │
│   └──► 通道三: 逐字节喂 OSC 状态机
│        ├── ESC ] 21337 ; status=generating BEL → Event::TabStatus
│        ├── ESC ] 9 ; 4 ; 1 ; 45 BEL          → Event::Progress
│        ├── ESC ] 0 ; title BEL               → Event::Title
│        └── BEL (0x07)                        → Event::Bell
│             │
│             └──► Outgoing::Event { ... } ──► ws_tx
│
├─ 线程 2 (JSONL 监视):
│   每 500ms:
│     fs::metadata(jsonl_path) → 检测文件大小增长
│     File::seek(SeekFrom::Start(last_size))
│     BufReader::lines() → 逐行 serde_json::from_str()
│       │
│       └──► Outgoing::Transcript { message } ──► ws_tx
│
├─ 线程 3 (WebSocket 读写):
│   tungstenite::connect(remote_url)
│   ├── read(): 远程客户端 → mpsc channel → 主线程
│   └── write(): mpsc channel ← 主线程/Pty/Jsonl → tungstenite::send()
│
└─ 主线程 (事件循环):
    while running:
      ├── jsonl_rx.try_recv() → ws_tx (通道二 → WebSocket)
      ├── ws_rx.try_recv()    → 解析 Incoming → pty.write() (远程 → PTY)
      └── sleep(50ms)
```

## 2. Cargo.toml

```toml
[package]
name = "claude-pty-controller"
version = "0.1.0"
edition = "2021"

[dependencies]
portable-pty = "0.8"       # PTY: wezterm 出品, Unix + Windows
tungstenite = "0.24"       # WebSocket: 纯 Rust, 无 OpenSSL
url = "2"                  # URL 解析
serde = { version = "1", features = ["derive"] }
serde_json = "1"
dirs = "5"                 # 获取 ~/.claude 路径
anyhow = "1"               # 错误处理
log = "0.4"
env_logger = "0.11"        # RUST_LOG=info
ctrlc = "3"                # SIGINT 优雅退出

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true               # 去掉调试符号 → ~3MB
```

**9 个直接依赖, 全链无 C/C++, 无 OpenSSL。**

## 3. 三条数据通道

和 Node.js 版完全一致的消息格式, 远程 Dashboard 零改动复用:

| 通道 | 数据源 | WebSocket JSON 消息 |
|------|--------|-------------------|
| **终端画面** | PTY stdout | `{"type":"output","raw":"\x1b[32mHello\x1b[0m\n"}` |
| **对话内容** | JSONL 文件 | `{"type":"transcript","message":{...}}` |
| **状态事件** | OSC 序列 | `{"type":"event","event":"tab_status","status":"generating"}` |

### 3a. 通道一: 终端画面

```rust
// PTY 读取线程
let mut buf = [0u8; 8192];
loop {
    let n = reader.read(&mut buf)?;    // 阻塞读
    if n == 0 { break; }

    let raw = String::from_utf8_lossy(&buf[..n]).to_string();

    // 直接转发, 不解析
    let msg = Outgoing::Output { raw };
    ws_tx.send(serde_json::to_string(&msg)?)?;
}
```

远程 Dashboard: `term.write(msg.raw)` → xterm.js 直接渲染。

### 3b. 通道二: 对话内容

JSONL 文件位置:

```
~/.claude/projects/<project-path>/<session-id>.jsonl
```

例如:
```
~/.claude/projects/-Users-sanchuan-myproject/cse_abc123def456.jsonl
```

每行一个结构化 JSON 消息:

```jsonl
{"type":"user","uuid":"req_001","message":{"role":"user","content":[{"type":"text","text":"帮我重构"}]}}
{"type":"assistant","uuid":"msg_001","message":{"role":"assistant","content":[{"type":"text","text":"好的"},{"type":"tool_use","id":"toolu_001","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","uuid":"msg_002","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_001","content":"..."}]}}
{"type":"assistant","uuid":"msg_003","message":{"role":"assistant","content":[{"type":"text","text":"我看到了这些文件..."}]}}
```

**关键:** 这段数据和你从 Agent SDK 拿到的完全一样。`tool_use.input` 是完整的 JSON 参数, `tool_result.content` 是工具的原始输出。

Watcher 实现:

```rust
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

// ── 找到最新 session 的 JSONL ──
fn find_latest_jsonl() -> Option<PathBuf> {
    let projects = dirs::home_dir()?.join(".claude").join("projects");
    if !projects.exists() { return None; }

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    walk_jsonl(&projects, &mut candidates);
    candidates.sort_by(|a, b| b.1.cmp(&a.1));  // mtime 倒序
    candidates.first().map(|(p, _)| p.clone())
}

fn walk_jsonl(dir: &PathBuf, out: &mut Vec<(PathBuf, std::time::SystemTime)>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_jsonl(&path, out);
            } else if let Some(ext) = path.extension() {
                if ext == "jsonl"
                    && !path.to_string_lossy().contains("timeline")  // 跳过 timeline.jsonl
                {
                    if let Ok(meta) = path.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            out.push((path, mtime));
                        }
                    }
                }
            }
        }
    }
}

// ── 监视循环 ──
fn watcher_loop(tx: Sender<Outgoing>, running: Arc<AtomicBool>) {
    let mut path = find_latest_jsonl();
    let mut last_size: u64 = path.as_ref()
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));

        // 文件还没被创建 (Claude Code 刚启动)
        if path.is_none() {
            path = find_latest_jsonl();
            last_size = 0;
            continue;
        }

        let p = path.as_ref().unwrap();
        let meta = match fs::metadata(p) {
            Ok(m) => m,
            Err(_) => {
                // 文件被删了 (session 结束) → 找下一个
                path = find_latest_jsonl();
                last_size = 0;
                continue;
            }
        };

        // 新 session 覆盖了老文件 (大小变小了)
        if meta.len() < last_size {
            // 先把旧文件的剩余内容读完
            if let Ok(file) = File::open(p) {
                read_lines_from(file, last_size, &tx);
            }
            path = find_latest_jsonl();
            last_size = 0;
            continue;
        }

        // 文件增长了 → 读增量
        if meta.len() > last_size {
            if let Ok(file) = File::open(p) {
                read_lines_from(file, last_size, &tx);
            }
            last_size = meta.len();
        }
    }
}

fn read_lines_from(file: File, start: u64, tx: &Sender<Outgoing>) {
    let mut f = file;
    if f.seek(SeekFrom::Start(start)).is_err() { return; }

    let reader = BufReader::new(f);
    for line in reader.lines().flatten() {
        if line.trim().is_empty() { continue; }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            let _ = tx.send(Outgoing::Transcript { message: value });
        }
    }
}
```

### 3c. 通道三: 状态事件

完整 OSC 协议 (从 Claude Code 源码提取):

#### Tab 状态 OSC 21337

```
ESC ] 21337 ; indicator=#00cc00 ; status=generating ; status-color=#ffffff BEL
```

| 字段 | 含义 | 示例值 |
|------|------|--------|
| `indicator` | 标签圆点颜色 (hex RGB) | `#00cc00` (绿), `#ffcc00` (黄), `#ff0000` (红), 空=清除 |
| `status` | 状态文本 | `generating`, `waiting`, `approval`, `idle` |
| `status-color` | 状态文字颜色 (hex RGB) | `#ffffff` |

#### 进度条 OSC 9;4 (iTerm2 协议)

```
ESC ] 9 ; 4 ; <op> ; <pct> BEL
```

| op | 含义 |
|----|------|
| 0 | 清除进度条 |
| 1 | 设置进度, pct=0-100 |
| 2 | 错误状态 |
| 3 | 不确定进度 (旋转) |

#### 窗口标题 OSC 0 / OSC 2

```
ESC ] 0 ; project-name — Claude Code BEL
ESC ] 2 ; Claude Code BEL
```

#### 通知铃 BEL

```
BEL (\x07) — 任务完成、权限请求、需要用户关注
```

#### 终端专属通知

| 终端 | 序列 |
|------|------|
| iTerm2 | `ESC ] 9 ; 0 ; <message> BEL` |
| Kitty | `ESC ] 99 ; i=<id>:p=title <title> BEL` |
| Ghostty | `ESC ] 777 ; notify ; <title> ; <message> BEL` |

#### tmux 包装

在 tmux 里运行时, OSC 被 DCS 包裹:

```
ESC P tmux ; <escaped_sequence> ESC \
```

内部 `ESC` 字符翻倍 (`\x1b\x1b`)。解析器需要先解包。

#### OSC 状态机实现

```rust
struct OscState {
    buf: String,      // 当前积累的字节
    in_osc: bool,     // 在 OSC 序列内 (ESC ] ...)
    in_dcs: bool,     // 在 DCS 序列内 (ESC P tmux; ...)
    dcs_buf: String,  // DCS 积累
}

#[derive(Debug)]
enum OscEvent {
    TabStatus  { status: Option<String> },
    Progress   { operation: String, percentage: Option<u32> },
    Title      { title: String },
    Bell,
}

impl OscState {
    fn feed(&mut self, data: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();

        for &byte in data {
            let ch = byte as char;

            // ── 在 DCS 内部 (tmux 包装) ──
            if self.in_dcs {
                self.dcs_buf.push(ch);
                // DCS 终止: ESC \
                if self.dcs_buf.ends_with("\x1b\\") {
                    self.in_dcs = false;
                    // 提取内部内容, 解包 ESC 翻倍
                    let inner = &self.dcs_buf[3..self.dcs_buf.len()-2];
                    let unescaped = inner.replace("\x1b\x1b", "\x1b");
                    // 递归解析
                    self.parse_osc_string(&unescaped, &mut events);
                    self.dcs_buf.clear();
                }
                continue;
            }

            // ── ESC 开始符 ──
            if ch == '\x1b' {
                self.buf = "\x1b".into();
                continue;
            }

            // ── OSC 开始: ESC ] ──
            if self.buf == "\x1b" && ch == ']' {
                self.in_osc = true;
                self.buf.push(ch);
                continue;
            }

            // ── DCS 开始: ESC P ──
            if self.buf == "\x1b" && ch == 'P' {
                self.in_dcs = true;
                self.dcs_buf = "\x1bP".into();
                self.buf.clear();
                continue;
            }

            // ── 在 OSC 内部: 积累字符直到终止符 ──
            if self.in_osc {
                self.buf.push(ch);
                // BEL (0x07) 或 ST (ESC \) 终止 OSC
                if byte == 0x07 || (ch == '\\' && self.buf.ends_with("\x1b\\")) {
                    self.in_osc = false;
                    self.parse_osc_sequence(&self.buf, &mut events);
                    self.buf.clear();
                }
                continue;
            }

            // ── 独立 BEL (不在 OSC 内) ──
            if byte == 0x07 && !self.in_osc {
                events.push(OscEvent::Bell);
            }

            self.buf.clear();
        }
        events
    }

    fn parse_osc_sequence(&self, seq: &str, events: &mut Vec<OscEvent>) {
        // 去掉 ESC ] 前缀和终止符 (BEL 或 ST)
        let inner = &seq[2..]
            .trim_end_matches('\x07')
            .trim_end_matches("\x1b\\");
        self.parse_osc_string(inner, events);
    }

    fn parse_osc_string(&self, inner: &str, events: &mut Vec<OscEvent>) {
        let semi = inner.find(';');
        let cmd: usize = semi.map_or_else(
            || inner.parse().unwrap_or(0),
            |i| inner[..i].parse().unwrap_or(0),
        );
        let data = semi.map_or("", |i| &inner[i+1..]);

        match cmd {
            0 | 2 => events.push(OscEvent::Title { title: data.to_string() }),
            9    => self.parse_iterm2(data, events),
            21337 => self.parse_tab_status(data, events),
            _    => {}  // 忽略未知 OSC 命令
        }
    }

    fn parse_iterm2(&self, data: &str, events: &mut Vec<OscEvent>) {
        let parts: Vec<&str> = data.split(';').collect();
        if parts.is_empty() { return; }

        match parts[0].parse::<u32>().unwrap_or(0) {
            4 => {  // Progress
                let op = parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                let pct = parts.get(2).and_then(|s| s.parse::<u32>().ok());
                let op_str = match op {
                    0 => "clear",
                    1 => "set",
                    2 => "error",
                    3 => "indeterminate",
                    _ => "unknown",
                };
                events.push(OscEvent::Progress {
                    operation: op_str.to_string(),
                    percentage: pct,
                });
            }
            _ => {}  // OSC 9;0 (iTerm2 通知) 等
        }
    }

    fn parse_tab_status(&self, data: &str, events: &mut Vec<OscEvent>) {
        let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for pair in data.split(';') {
            if let Some(eq) = pair.find('=') {
                fields.insert(pair[..eq].to_string(), pair[eq+1..].to_string());
            }
        }
        events.push(OscEvent::TabStatus {
            status: fields.get("status").cloned(),
        });
    }
}
```

使用:

```rust
let mut osc = OscState::new();

// 在 PTY 读线程里:
let events = osc.feed(&buf[..n]);
for event in events {
    let msg = match event {
        OscEvent::TabStatus { status } => Outgoing::Event {
            event: "tab_status".into(), status, ..
        },
        OscEvent::Progress { operation, percentage } => Outgoing::Event {
            event: "progress".into(), operation: Some(operation), percentage, ..
        },
        OscEvent::Title { title } => Outgoing::Event {
            event: "title".into(), title: Some(title), ..
        },
        OscEvent::Bell => Outgoing::Event {
            event: "bell".into(), ..
        },
    };
    let _ = ws_tx.send(serde_json::to_string(&msg)?);
}
```

## 4. PTY 管理

```rust
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct PtySession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl PtySession {
    fn spawn(
        shell: &str,
        cwd: &str,
        envs: &[(&str, &str)],
    ) -> anyhow::Result<(Self, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 45,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        for (k, v) in envs {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd)?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        Ok((PtySession { writer, child }, reader))
    }

    fn write(&mut self, text: &str) -> std::io::Result<()> {
        self.writer.write_all(text.as_bytes())?;
        self.writer.flush()
    }
}
```

## 5. 输入注入

```rust
// 在主线程里处理远程发来的命令:
match msg {
    Incoming::Input { text } => {
        pty.write(&format!("{}\r", text))?;
    }
    Incoming::Raw { text } => {
        // 直接写入控制字符
        // \x03 = Ctrl+C, \x04 = Ctrl+D, \x1b[A = ↑
        pty.write(&text)?;
    }
    Incoming::Resize { cols, rows } => {
        // 发送 ANSI resize 序列
        pty.write(&format!("\x1b[8;{};{}t", rows, cols))?;
    }
}
```

远程 WebSocket 消息:

```json
{"type":"input","text":"帮我重构这个模块"}
{"type":"raw","text":"\x03"}
{"type":"raw","text":"\x1b[A"}
{"type":"resize","cols":200,"rows":50}
```

## 6. WebSocket 管理

```rust
use tungstenite::{connect, Message};
use url::Url;

struct WsHandle {
    write_tx: Sender<String>,         // → WebSocket 写线程
    read_rx: mpsc::Receiver<String>,  // ← WebSocket 读线程
    connected: Arc<AtomicBool>,
}

fn connect_ws(url: &str) -> anyhow::Result<WsHandle> {
    let (write_tx, write_rx) = mpsc::channel::<String>();
    let (read_tx, read_rx) = mpsc::channel::<String>();
    let connected = Arc::new(AtomicBool::new(false));
    let url = url.to_string();

    thread::spawn(move || {
        loop {
            // 连接, 失败则 2 秒后重试
            match connect(Url::parse(&url).unwrap()) {
                Ok((mut ws, _)) => {
                    connected.store(true, Ordering::Relaxed);

                    // 写子线程
                    let ws_writer = ws.get_mut().try_clone().expect("clone");
                    let writer = Arc::new(Mutex::new(ws_writer));
                    let done = Arc::new(AtomicBool::new(false));
                    let writer_clone = writer.clone();
                    let done_clone = done.clone();

                    thread::spawn(move || {
                        while !done_clone.load(Ordering::Relaxed) {
                            if let Ok(msg) = write_rx.recv_timeout(Duration::from_millis(100)) {
                                if let Ok(mut s) = writer_clone.lock() {
                                    let _ = s.write_all(msg.as_bytes());
                                    let _ = s.flush();
                                }
                            }
                        }
                    });

                    // 读循环
                    loop {
                        match ws.read() {
                            Ok(Message::Text(text)) => {
                                if read_tx.send(text).is_err() { break; }
                            }
                            Ok(Message::Close(_)) => break,
                            Err(_) => break,
                            _ => {}
                        }
                    }
                    done.store(true, Ordering::Relaxed);
                }
                Err(_) => thread::sleep(Duration::from_secs(2)),
            }
            connected.store(false, Ordering::Relaxed);
        }
    });

    Ok(WsHandle { write_tx, read_rx, connected })
}
```

## 7. 主事件循环

```rust
fn main() -> anyhow::Result<()> {
    env_logger::init();

    let running = Arc::new(AtomicBool::new(true));

    // SIGINT / SIGTERM → 优雅退出
    ctrlc::set_handler({
        let r = running.clone();
        move || { r.store(false, Ordering::Relaxed); }
    })?;

    // ── WebSocket ──
    let remote_url = std::env::var("REMOTE_URL")
        .unwrap_or_else(|_| "ws://localhost:9000".into());
    let ws = connect_ws(&remote_url)?;

    // ── JSONL Watcher ──
    let (jsonl_tx, jsonl_rx) = mpsc::channel();
    start_jsonl_watcher(jsonl_tx, running.clone());

    // ── PTY ──
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let envs = vec![
        ("ANTHROPIC_BASE_URL", &std::env::var("ANTHROPIC_BASE_URL").unwrap_or_default()),
        ("ANTHROPIC_API_KEY", &std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()),
        ("TERM", "xterm-256color"),
    ];
    let (mut pty, reader) = PtySession::spawn(&shell, &cwd, &envs.iter().map(|(k,v)| (*k, v.as_str())).collect::<Vec<_>>())?;

    // 在 shell 里启动 claude
    pty.write("claude\r")?;

    // ── PTY 读线程 ──
    let ws_tx = ws.write_tx.clone();
    let r = running.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut osc = OscState::new();
        loop {
            if !r.load(Ordering::Relaxed) { break; }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    // 通道一: 终端画面
                    let _ = ws_tx.send(serde_json::to_string(
                        &Outgoing::Output { raw }
                    ).unwrap_or_default());
                    // 通道三: OSC 状态事件
                    for event in osc.feed(&buf[..n]) {
                        let msg = event_to_outgoing(event);
                        let _ = ws_tx.send(serde_json::to_string(&msg).unwrap_or_default());
                    }
                }
                Err(_) => break,
            }
        }
    });

    // ── 主循环 ──
    while running.load(Ordering::Relaxed) {
        // JSONL → WebSocket
        while let Ok(msg) = jsonl_rx.try_recv() {
            ws.send(&msg);
        }
        // WebSocket → PTY
        while let Some(raw) = ws.recv() {
            if let Ok(msg) = serde_json::from_str::<Incoming>(&raw) {
                handle_incoming(&mut pty, msg);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    // 清理
    pty.write("\x04").ok();  // Ctrl+D 退出 shell
    log::info!("Shutdown");
    Ok(())
}
```

## 8. 编译部署

```bash
# 编译 (release)
cd claude-pty-controller
cargo build --release
# → target/release/claude-pty-controller  (~3MB, 静态链接)

# 部署
scp target/release/claude-pty-controller user@server:/opt/

# 运行
REMOTE_URL=ws://your-relay:9000 \
ANTHROPIC_BASE_URL=https://your-proxy.com \
ANTHROPIC_API_KEY=sk-xxx \
RUST_LOG=info \
/opt/claude-pty-controller
```

## 9. 和 Node.js 版对比

| | Node.js 版 | Rust 版 |
|---|---|---|
| 运行时 | Node 18+ | 无 |
| 编译产物 | .js + node_modules/ (~200MB) | 单一 ~3MB 二进制 |
| 部署 | npm install (网络 + 编译 node-pty) | scp 一个文件 |
| 内存 | ~40MB | ~8MB |
| CPU（空闲） | ~0.5% | ~0.1% |
| PTY | node-pty (C++ addon) | portable-pty (纯 Rust) |
| WebSocket | ws (npm) | tungstenite (纯 Rust) |
| 跨平台 | ✅ | ✅ |
| 协议兼容 | ✅ | ✅ 完全兼容, 同一套 WebSocket 消息 |
| 远程 Dashboard | ✅ | ✅ 零改动复用 |

**远程 Dashboard 的 HTML/JS 完全不变。** 换 Controller 对前端透明 — JSON 消息格式一模一样。
