# Dashboard Protocol Contract

The control-end (Dashboard) contract — everything a front-end needs to connect,
authenticate, decrypt, render, and drive a session, **without reading the
controller internals**. Normative schema: ARCHITECTURE §16.3. Reference
implementation: [`dashboard/cli.mjs`](../dashboard/cli.mjs) (+ `noise.mjs`),
verified to interoperate with the Rust controller in `tests/controller_e2ee.rs`
and `dashboard/interop-test.sh`.

There are two transports. **Both carry the same application messages (§3).**

- **Relay + E2EE (default).** Both controller and dashboard dial OUT to a relay;
  the relay routes opaque ciphertext. The dashboard runs a Noise handshake with
  the controller and encrypts/decrypts every app message. **This is the only way
  to reach a controller that's behind a relay** (the controller speaks only
  ciphertext to dashboards).
- **Direct wss (no relay).** A dashboard connects straight to a reachable
  controller's `wss://` with a bearer token; app messages are plaintext JSON
  (TLS only). v1's controller is a relay *client*, so this mode needs the
  controller to expose a server (not built yet) — treat as future.

## 1. Relay envelope layer

The endpoint↔relay link carries JSON envelopes (`relay_proto.rs`). The relay
never inspects `data` (base64 of opaque E2EE bytes).

```jsonc
// dashboard → relay (first frame)
{"t":"join","room":"<room-id>","role":"dashboard","peer":"<your-id>","token":"<RELAY_TOKEN>"}
// relay → dashboard
{"t":"joined"}                                  // ack
{"t":"peer_join","peer":"controller"}           // controller is present → start handshake
{"t":"peer_leave","peer":"controller"}          // controller gone → tear down session
{"t":"deliver","from":"controller","data":"<base64>"}   // a frame from the controller
{"t":"error","msg":"…"}
// dashboard → relay (after join)
{"t":"msg","to":"controller","data":"<base64>"} // a frame for the controller
```

**`room-id`** is derived from the shared `PAIRING_SECRET` (so the relay can pair
you without knowing the secret):

```
rendezvous_secret = HMAC-SHA256("claude-pty-controller/rendezvous/v1", PAIRING_SECRET)
epoch    = floor(unix_seconds / 300)
room-id  = hex( HMAC-SHA256(rendezvous_secret, "rendezvous" || u64_be(epoch)) [0..16] )
```

Try `epoch-1, epoch, epoch+1` to tolerate clock skew. (`noise.mjs` exposes
`roomIdNow(pairing)`.)

## 2. E2EE handshake (Noise `XXpsk3_25519_ChaChaPoly_SHA256`)

The **dashboard is the initiator**, the controller the responder. PSK =
`HMAC-SHA256("claude-pty-controller/psk/v1", PAIRING_SECRET)`. Each side has a
long-term X25519 static keypair.

```
on peer_join{controller}:   m1 = writeMsg1();           send msg(to=controller, m1)
on deliver(m2):             readMsg2(m2); m3 = writeMsg3(); send msg(to=controller, m3)
                            → transport ready; pin remoteStatic() = controller pubkey
on deliver(ciphertext):     plaintext = decrypt(ciphertext) → an app message (§3)
to send an app message:     send msg(to=controller, encrypt(json))
```

- **First connect (enrollment):** the controller adds your static public key to
  its authorized list **only while it's in pairing mode** (`CPC_ALLOW_ENROLL`).
  Afterwards, only authorized keys connect; a removed key is rejected even though
  it knows the PSK (that's how **revocation** works).
- **`PAIRING_SECRET` must be high-entropy** (≥32 chars, e.g. `openssl rand -hex
  32`). Noise+PSK is not a PAKE — a weak secret is offline-guessable by an active
  relay.
- The relay only ever sees `room-id`, ciphertext sizes, and timing.

A full, interoperable JS implementation is `dashboard/noise.mjs`.

## 3. Application messages (normative, §16.3)

After the handshake, every `deliver`/`msg` payload (decrypted) is one of these.
`v` is the schema version; **ignore unknown fields and unknown `kind`/`state`/
`type` values** (additive changes keep `v:1`).

### Controller → Dashboard (outbound)

```jsonc
// Capabilities — sent once right after your E2EE comes up, and on agent/session change.
{"type":"hello","v":1,"agent":"claude",
 "capabilities":{"transcript":true,"status":true,"multi_session":false,"input":true}}

// Channel 1 — terminal screen bytes (valid UTF-8). Feed straight to xterm.js.
{"type":"output","raw":"[32mhi[0m\r\n"}

// Channel 2 — one transcript event. Dedup on (msg_uuid, part_index).
{"type":"transcript","v":1,"agent":"claude","session":"<id>","role":"user|assistant|tool|system",
 "parts":[{"kind":"text","text":"…"}
        | {"kind":"thinking","text":"…"}
        | {"kind":"tool_use","id":"…","name":"Bash","input":{…}}
        | {"kind":"tool_result","forId":"…","content":"… | [blocks]"}],
 "msg_uuid":"…","part_index":0,"raw":{…}}

// Channel 3 — steady status (drive a status chip) and one-shot notifications.
{"type":"event","v":1,"agent":"claude","session":"<id>","state":"idle|working|waiting"}
{"type":"notify","v":1,"agent":"claude","session":"<id>"}     // bell — best-effort, NOT a turn boundary

// Session boundary — clear/rebuild the conversation view for the new session.
{"type":"session","session_id":"<id>","cwd":"…","path":"…","reason":"new|resume|switch"}
```

### Dashboard → Controller (inbound)

```jsonc
{"type":"input","text":"refactor this module"}   // submitted text (controller appends the agent's submit key)
{"type":"raw","text":""}                    // raw control bytes: Ctrl-C, arrows ([A), …
{"type":"resize","cols":200,"rows":50}            // PTY resize
{"type":"refresh","scope":"tail|full"}            // re-send channel-2: tail=incremental, full=from start
{"type":"auth","token":"…"}                       // direct mode only (bearer); ignored under E2EE
```

## 4. Behaviors a dashboard MUST implement

- **On connect (after E2EE up):** you'll get `hello`. Send `{"type":"refresh","scope":"full"}`
  to rebuild the transcript (you only receive frames produced after your session
  is ready, so history needs an explicit pull).
- **Dedup:** transcript events carry `(msg_uuid, part_index)`; drop duplicates
  (a restart or `refresh:full` re-sends). `output` is a stream — never dedup it.
- **Reconnect:** on disconnect, re-`join` (recompute `room-id` for the current
  epoch), re-handshake, re-`refresh:full`. Backoff with jitter.
- **Session switch:** on `{"type":"session",…}`, clear the transcript view and
  start fresh; keep channel-1 as a live terminal.
- **Driving / single-writer:** any authorized dashboard may send input, but the
  controller honors only the current **driver** (others are view-only). Driver
  hand-off UX is up to the front-end; a non-driver's `input` is dropped. A local
  `tmux attach` is an independent writer the controller can't gate.
- **Rendering:** `output` → xterm.js `term.write(raw)`; `transcript.parts` → a
  message list (render `thinking` distinctly; `tool_use`/`tool_result` as
  collapsible); `event.state` → a status chip (idle/working/waiting); `notify` →
  a transient toast/bell.

## 5. Reference & verification

- `dashboard/noise.mjs` — Noise XXpsk3 over `node:crypto`, byte-compatible with
  the Rust controller's `snow`.
- `dashboard/cli.mjs` — a runnable terminal dashboard (prints decrypted frames,
  forwards stdin as `input`).
- `dashboard/index.html` — a browser dashboard (xterm.js) speaking this protocol.
- `dashboard/interop-test.sh` — spins up the real relay + controller and drives
  the Node dashboard against them, asserting it decrypts channel-1 output.
