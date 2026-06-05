# Dashboard (control-end)

Reference implementations of the [dashboard protocol](../docs/PROTOCOL.md). Both
do the relay join + Noise XXpsk3 E2EE handshake with the controller and
encrypt/decrypt every app message — the relay only sees ciphertext.

## Browser (`index.html`)

xterm.js terminal (channel 1) + transcript pane + status chip + input box.
Serve it (WebCrypto needs a secure context — `localhost` counts):

```sh
cd dashboard && python3 -m http.server 8080
# open http://localhost:8080/  → fill in Relay URL / PAIRING_SECRET / RELAY_TOKEN → Connect
```

Crypto: `noise-web.mjs` (WebCrypto X25519/HMAC/SHA-256 + the vendored
`chacha20poly1305.mjs`, since WebCrypto has no ChaCha). Needs a browser with
WebCrypto X25519 (Chrome ≥133, recent Firefox) or run via Node.

## Terminal (`cli.mjs`, Node ≥ 20)

Prints one decoded message per line and forwards stdin as `input`:

```sh
PAIRING_SECRET=… RELAY_TOKEN=… node cli.mjs --url ws://relay:9000
```

Crypto: `noise.mjs` (Node `crypto`). This is the reference verified against the
Rust controller's `snow` in `interop-test.sh` and `tests/controller_e2ee.rs`.

## Verify

```sh
cargo build --bins
./interop-test.sh                 # real relay + controller + this dashboard, end-to-end
node --test chacha20poly1305.test.mjs   # vendored AEAD vs node:crypto
```

## Pairing

First connect enrolls the dashboard's static key while the controller runs with
`CPC_ALLOW_ENROLL=1`. Turn it off afterwards; a removed key is then rejected even
though it knows the PAIRING_SECRET (per-device revocation).
