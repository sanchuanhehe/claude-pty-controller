# claude-pty-controller

Remotely observe and drive a [Claude Code](https://claude.com/claude-code)
terminal session over the network — end-to-end encrypted, NAT-friendly, and
agent-agnostic.

claude runs inside a **tmux** session, so the controller/SSH dropping doesn't
kill it; a local terminal can `tmux attach` and share the same session,
bidirectionally and transparently, with the remote dashboard. The controller is
a single self-contained Rust binary (`scp` one file; dynamically links glibc —
use the `musl` target for fully static).

```
┌─ Dashboard ─┐        ┌──── Relay ────┐        ┌──── Controller (this repo) ────┐
│ browser /   │─wss/TLS│ zero-knowledge│ wss/TLS│ tmux + claude, 3 channels,     │
│ CLI         │◀──────▶│ room router   │◀──────▶│ input injection                │
└─────────────┘        └───────────────┘        └────────────────────────────────┘
        └────────── E2EE (Noise XXpsk3), opaque to the relay ──────────┘
```

- **Three tiers.** Controller and Dashboard both dial *out* to the Relay (no
  inbound port on the claude host). The relay only forwards **opaque ciphertext**
  — it can't read the traffic.
- **End-to-end encryption.** Per-dashboard `Noise_XXpsk3_25519_ChaChaPoly_SHA256`
  sessions; per-device static keys with revocation. The pairing secret must be
  high-entropy. (ARCHITECTURE.md §13–§14.)
- **Agent-agnostic.** Channel 1 (screen) works for any TUI with zero adapter;
  channels 2/3 are an `AgentAdapter` — claude is the first. (§16.)

## Three data channels → one normalized schema

| Channel | Source | Normalized message ([PROTOCOL](docs/PROTOCOL.md)) |
|---------|--------|---------------------------------------------------|
| Terminal screen | PTY stdout | `{"type":"output","raw":"…"}` |
| Conversation | JSONL | `{"type":"transcript","role":…,"parts":[…],"msg_uuid":…}` |
| Status | OSC | `{"type":"event","state":"idle\|working\|waiting"}` |

Inbound (dashboard → controller): `input` / `raw` / `resize` / `refresh`.
Channel 2 refreshes on a poll, on turn-end (status transition), and on manual
`refresh`.

## Quick start

```sh
cargo build --release        # → target/release/{claude-pty-controller,relay}
```

**1. Relay** (a small public-reachable host):

```sh
RELAY_ADDR=0.0.0.0:9000 RELAY_TOKEN=$(openssl rand -hex 16) ./relay
```

**2. Controller** (the claude host) — relay + E2EE mode is selected by
`PAIRING_SECRET`:

```sh
PAIRING_SECRET=$(openssl rand -hex 32)   # share with the dashboard out-of-band
REMOTE_URL=wss://your-relay:9000 \
RELAY_TOKEN=… \
PAIRING_SECRET=$PAIRING_SECRET \
CPC_ALLOW_ENROLL=1 \                      # only while pairing a new device
ANTHROPIC_API_KEY=sk-… \
./claude-pty-controller
```

**3. Dashboard** ([dashboard/](dashboard/)) — browser or terminal:

```sh
# browser (WebCrypto needs a secure context; localhost counts)
cd dashboard && python3 -m http.server 8080
# open http://localhost:8080/ → enter Relay URL / PAIRING_SECRET / RELAY_TOKEN → Connect

# or terminal
PAIRING_SECRET=… RELAY_TOKEN=… node dashboard/cli.mjs --url wss://your-relay:9000
```

First connect enrolls the dashboard's device key (while `CPC_ALLOW_ENROLL=1`);
turn it off afterwards — a removed key is then rejected even though it knows the
pairing secret (per-device revocation).

**Direct mode** (no relay): set `CONTROL_TOKEN` instead of `PAIRING_SECRET` and
point `REMOTE_URL` at a `wss://` endpoint (ARCHITECTURE.md §5).

Run as a service: see [deploy/](deploy/) (systemd `Type=notify` + watchdog).

## Status

Implemented and tested: the five core milestones (ARCHITECTURE.md §11) — tmux
session host, all three channels, auth+wss, relay + per-dashboard E2EE fan-out,
robustness (single-instance lock, channel-aware backpressure, graceful detach) —
plus the systemd watchdog and a verified browser/CLI dashboard. The JS dashboard
crypto interoperates byte-for-byte with the Rust controller (`snow`).

Remaining work is tracked in [issues](https://github.com/sanchuanhehe/claude-pty-controller/issues)
(secret redaction, protocol versioning, observability, vt100 snapshot, native
Windows).

## Layout

```
src/                 controller (lib + bin) and src/bin/relay.rs
dashboard/           browser (index.html) + CLI (cli.mjs) reference dashboards
deploy/              systemd unit + env example
docs/ARCHITECTURE.md the design (env-verified, two rounds of review)
docs/PROTOCOL.md     the dashboard contract
```

## Security

The controller can inject input into a shell that holds `ANTHROPIC_API_KEY` —
treat it as remote-shell access. It **refuses to start** without auth
(`PAIRING_SECRET` or `CONTROL_TOKEN`; loopback may use `CPC_INSECURE=1`) and
enforces `wss://` off loopback. The relay is untrusted by design — content is
E2EE. See ARCHITECTURE.md §5/§14.

## Build & test

```sh
cargo test                          # 32 unit + 2 E2EE-through-relay integration tests
./dashboard/interop-test.sh         # real relay + controller + Node dashboard, end-to-end
node --test dashboard/chacha20poly1305.test.mjs
```
