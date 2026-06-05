// Compact ChaCha20-Poly1305 AEAD (RFC 8439), pure JS — for browsers (WebCrypto
// has no ChaCha). Verified against node:crypto in chacha20poly1305.test.mjs.
// Operates on Uint8Array. Nonce = 12 bytes.

function rotl(a, b) { return ((a << b) | (a >>> (32 - b))) >>> 0; }

function chacha20Block(key, counter, nonce) {
  const c = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];
  const k = new Uint32Array(8), n = new Uint32Array(3);
  const dv = (arr) => new DataView(arr.buffer, arr.byteOffset, arr.byteLength);
  for (let i = 0; i < 8; i++) k[i] = dv(key).getUint32(i * 4, true);
  for (let i = 0; i < 3; i++) n[i] = dv(nonce).getUint32(i * 4, true);
  const s = new Uint32Array([c[0], c[1], c[2], c[3], k[0], k[1], k[2], k[3], k[4], k[5], k[6], k[7], counter >>> 0, n[0], n[1], n[2]]);
  const x = s.slice();
  const QR = (a, b, cc, d) => {
    x[a] = (x[a] + x[b]) >>> 0; x[d] = rotl(x[d] ^ x[a], 16);
    x[cc] = (x[cc] + x[d]) >>> 0; x[b] = rotl(x[b] ^ x[cc], 12);
    x[a] = (x[a] + x[b]) >>> 0; x[d] = rotl(x[d] ^ x[a], 8);
    x[cc] = (x[cc] + x[d]) >>> 0; x[b] = rotl(x[b] ^ x[cc], 7);
  };
  for (let i = 0; i < 10; i++) {
    QR(0, 4, 8, 12); QR(1, 5, 9, 13); QR(2, 6, 10, 14); QR(3, 7, 11, 15);
    QR(0, 5, 10, 15); QR(1, 6, 11, 12); QR(2, 7, 8, 13); QR(3, 4, 9, 14);
  }
  const out = new Uint8Array(64), odv = new DataView(out.buffer);
  for (let i = 0; i < 16; i++) odv.setUint32(i * 4, (x[i] + s[i]) >>> 0, true);
  return out;
}

function chacha20(key, counter, nonce, data) {
  const out = new Uint8Array(data.length);
  for (let i = 0; i < data.length; i += 64) {
    const ks = chacha20Block(key, counter + (i / 64), nonce);
    for (let j = 0; j < 64 && i + j < data.length; j++) out[i + j] = data[i + j] ^ ks[j];
  }
  return out;
}

// Poly1305 (RFC 8439) using BigInt for the 130-bit math (simple, correct).
function poly1305(key, msg) {
  const P = (1n << 130n) - 5n;
  let r = bytesToLE(key.subarray(0, 16));
  r &= 0x0ffffffc0ffffffc0ffffffc0fffffffn;
  const s = bytesToLE(key.subarray(16, 32));
  let acc = 0n;
  for (let i = 0; i < msg.length; i += 16) {
    const block = msg.subarray(i, i + 16);
    let n = bytesToLE(block) + (1n << BigInt(block.length * 8));
    acc = ((acc + n) * r) % P;
  }
  acc = (acc + s) & ((1n << 128n) - 1n);
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) { out[i] = Number(acc & 0xffn); acc >>= 8n; }
  return out;
}

function bytesToLE(b) { let n = 0n; for (let i = b.length - 1; i >= 0; i--) n = (n << 8n) | BigInt(b[i]); return n; }
function pad16(len) { return (16 - (len % 16)) % 16; }
function u64le(n) { const b = new Uint8Array(8), dv = new DataView(b.buffer); dv.setBigUint64(0, BigInt(n), true); return b; }

function polyKey(key, nonce) { return chacha20(key, 0, nonce, new Uint8Array(32)); }

function tag(key, nonce, aad, ct) {
  const mac = new Uint8Array(aad.length + pad16(aad.length) + ct.length + pad16(ct.length) + 16);
  let o = 0;
  mac.set(aad, o); o += aad.length + pad16(aad.length);
  mac.set(ct, o); o += ct.length + pad16(ct.length);
  mac.set(u64le(aad.length), o); o += 8;
  mac.set(u64le(ct.length), o);
  return poly1305(polyKey(key, nonce), mac);
}

export function seal(key, nonce, aad, plaintext) {
  const ct = chacha20(key, 1, nonce, plaintext);
  const out = new Uint8Array(ct.length + 16);
  out.set(ct); out.set(tag(key, nonce, aad, ct), ct.length);
  return out;
}

export function open(key, nonce, aad, ciphertext) {
  const ct = ciphertext.subarray(0, ciphertext.length - 16);
  const t = ciphertext.subarray(ciphertext.length - 16);
  const expect = tag(key, nonce, aad, ct);
  let diff = 0;
  for (let i = 0; i < 16; i++) diff |= t[i] ^ expect[i];
  if (diff !== 0) throw new Error('chacha20poly1305: auth failed');
  return chacha20(key, 1, nonce, ct);
}
