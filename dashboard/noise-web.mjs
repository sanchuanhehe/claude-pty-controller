// Noise_XXpsk3_25519_ChaChaPoly_SHA256 — browser (WebCrypto) backend.
// Same protocol as noise.mjs, async (WebCrypto subtle) + the vendored ChaCha.
// Runs in modern browsers (WebCrypto X25519) and Node 22. See docs/PROTOCOL.md.
import { seal, open } from './chacha20poly1305.mjs';

const subtle = globalThis.crypto.subtle;
const NAME = 'Noise_XXpsk3_25519_ChaChaPoly_SHA256';
const PKCS8 = hex('302e020100300506032b656e04220420');

function hex(s) { const b = new Uint8Array(s.length / 2); for (let i = 0; i < b.length; i++) b[i] = parseInt(s.substr(i * 2, 2), 16); return b; }
function concat(...a) { const n = a.reduce((s, x) => s + x.length, 0); const o = new Uint8Array(n); let p = 0; for (const x of a) { o.set(x, p); p += x.length; } return o; }
const enc = (s) => new TextEncoder().encode(s);

async function sha256(d) { return new Uint8Array(await subtle.digest('SHA-256', d)); }
async function hmac(key, d) {
  const k = await subtle.importKey('raw', key, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return new Uint8Array(await subtle.sign('HMAC', k, d));
}
async function hkdf(ck, ikm, n) {
  const tmp = await hmac(ck, ikm);
  const o1 = await hmac(tmp, Uint8Array.of(1));
  const o2 = await hmac(tmp, concat(o1, Uint8Array.of(2)));
  if (n === 2) return [o1, o2];
  const o3 = await hmac(tmp, concat(o2, Uint8Array.of(3)));
  return [o1, o2, o3];
}
async function importPriv(raw) { return subtle.importKey('pkcs8', concat(PKCS8, raw), { name: 'X25519' }, false, ['deriveBits']); }
async function importPub(raw) { return subtle.importKey('raw', raw, { name: 'X25519' }, false, []); }
async function dh(privRaw, pubRaw) {
  const pk = await importPriv(privRaw), pub = await importPub(pubRaw);
  return new Uint8Array(await subtle.deriveBits({ name: 'X25519', public: pub }, pk, 256));
}
export async function genKeypair() {
  const kp = await subtle.generateKey({ name: 'X25519' }, true, ['deriveBits']);
  const pub = new Uint8Array(await subtle.exportKey('raw', kp.publicKey));
  const priv = new Uint8Array(await subtle.exportKey('pkcs8', kp.privateKey)).subarray(PKCS8.length);
  return { priv, pub };
}
function nonce(n) { const b = new Uint8Array(12); new DataView(b.buffer).setBigUint64(4, BigInt(n), true); return b; }

class Sym {
  static async create() {
    const s = new Sym();
    const nb = enc(NAME);
    s.h = nb.length <= 32 ? concat(nb, new Uint8Array(32 - nb.length)) : await sha256(nb);
    s.ck = s.h.slice(); s.k = null; s.n = 0;
    return s;
  }
  async mixHash(d) { this.h = await sha256(concat(this.h, d)); }
  async mixKey(ikm) { const [ck, k] = await hkdf(this.ck, ikm, 2); this.ck = ck; this.k = k.subarray(0, 32); this.n = 0; }
  async mixKeyAndHash(ikm) { const [ck, h2, k] = await hkdf(this.ck, ikm, 3); this.ck = ck; await this.mixHash(h2); this.k = k.subarray(0, 32); this.n = 0; }
  async encryptAndHash(pt) { const ct = this.k ? seal(this.k, nonce(this.n++), this.h, pt) : pt; await this.mixHash(ct); return ct; }
  async decryptAndHash(ct) { const pt = this.k ? open(this.k, nonce(this.n++), this.h, ct) : ct; await this.mixHash(ct); return pt; }
  async split() { const [t1, t2] = await hkdf(this.ck, new Uint8Array(0), 2); return [new Cph(t1.subarray(0, 32)), new Cph(t2.subarray(0, 32))]; }
  hasKey() { return this.k != null; }
}
class Cph {
  constructor(k) { this.k = k; this.n = 0; }
  encrypt(pt) { return seal(this.k, nonce(this.n++), new Uint8Array(0), pt); }
  decrypt(ct) { return open(this.k, nonce(this.n++), new Uint8Array(0), ct); }
}

export class Initiator {
  constructor(staticPriv, staticPub, psk) { this.s = { priv: staticPriv, pub: staticPub }; this.psk = psk; this.up = false; }
  async init() { this.sym = await Sym.create(); await this.sym.mixHash(new Uint8Array(0)); return this; }
  async writeMsg1() {
    this.e = await genKeypair();
    await this.sym.mixHash(this.e.pub);
    await this.sym.mixKey(this.e.pub);
    return concat(this.e.pub, await this.sym.encryptAndHash(new Uint8Array(0)));
  }
  async readMsg2(msg) {
    let i = 0;
    this.re = msg.subarray(i, i + 32); i += 32;
    await this.sym.mixHash(this.re);
    await this.sym.mixKey(this.re);
    await this.sym.mixKey(await dh(this.e.priv, this.re)); // ee
    const slen = this.sym.hasKey() ? 48 : 32;
    this.rs = await this.sym.decryptAndHash(msg.subarray(i, i + slen)); i += slen;
    await this.sym.mixKey(await dh(this.e.priv, this.rs)); // es
    await this.sym.decryptAndHash(msg.subarray(i));        // payload
  }
  async writeMsg3() {
    const sct = await this.sym.encryptAndHash(this.s.pub);
    await this.sym.mixKey(await dh(this.s.priv, this.re)); // se
    await this.sym.mixKeyAndHash(this.psk);                // psk
    const payload = await this.sym.encryptAndHash(new Uint8Array(0));
    const [send, recv] = await this.sym.split();
    this.send = send; this.recv = recv; this.up = true;
    return concat(sct, payload);
  }
  remoteStatic() { return this.rs; }
  encrypt(pt) { return this.send.encrypt(pt); }
  decrypt(ct) { return this.recv.decrypt(ct); }
}

export async function derivePsk(pairing) { return hmac(enc('claude-pty-controller/psk/v1'), enc(pairing)); }
export async function roomIdNow(pairing, nowSecs = Math.floor(Date.now() / 1000)) {
  const rs = await hmac(enc('claude-pty-controller/rendezvous/v1'), enc(pairing));
  const epoch = Math.floor(nowSecs / 300);
  const eb = new Uint8Array(8); new DataView(eb.buffer).setBigUint64(0, BigInt(epoch), false);
  const mac = await hmac(rs, concat(enc('rendezvous'), eb));
  return [...mac.subarray(0, 16)].map((b) => b.toString(16).padStart(2, '0')).join('');
}
