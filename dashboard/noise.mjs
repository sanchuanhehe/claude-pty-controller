// Noise_XXpsk3_25519_ChaChaPoly_SHA256 — dashboard (initiator) side.
// Interoperates byte-for-byte with the Rust controller's `snow` responder.
// Node backend (node:crypto). See docs/PROTOCOL.md §E2EE.
import crypto from 'node:crypto';

const SPKI = Buffer.from('302a300506032b656e032100', 'hex');   // X25519 SPKI prefix
const PKCS8 = Buffer.from('302e020100300506032b656e04220420', 'hex'); // X25519 PKCS8 prefix
const NAME = 'Noise_XXpsk3_25519_ChaChaPoly_SHA256';

const sha256 = (d) => crypto.createHash('sha256').update(d).digest();
const hmac = (k, d) => crypto.createHmac('sha256', k).update(d).digest();

// Noise HKDF (2 or 3 outputs).
function hkdf(ck, ikm, n) {
  const tmp = hmac(ck, ikm);
  const o1 = hmac(tmp, Buffer.from([1]));
  const o2 = hmac(tmp, Buffer.concat([o1, Buffer.from([2])]));
  if (n === 2) return [o1, o2];
  const o3 = hmac(tmp, Buffer.concat([o2, Buffer.from([3])]));
  return [o1, o2, o3];
}

function importPub(raw) {
  return crypto.createPublicKey({ key: Buffer.concat([SPKI, raw]), format: 'der', type: 'spki' });
}
function importPriv(raw) {
  return crypto.createPrivateKey({ key: Buffer.concat([PKCS8, raw]), format: 'der', type: 'pkcs8' });
}
function dh(privRaw, pubRaw) {
  return crypto.diffieHellman({ privateKey: importPriv(privRaw), publicKey: importPub(pubRaw) });
}
export function genKeypair() {
  const kp = crypto.generateKeyPairSync('x25519');
  const pub = Buffer.from(kp.publicKey.export({ type: 'spki', format: 'der' }).subarray(SPKI.length));
  const priv = Buffer.from(kp.privateKey.export({ type: 'pkcs8', format: 'der' }).subarray(PKCS8.length));
  return { priv, pub };
}

function nonce(n) {
  const b = Buffer.alloc(12);
  b.writeBigUInt64LE(BigInt(n), 4); // 4 zero bytes ++ LE u64
  return b;
}
function aeadEnc(k, n, ad, pt) {
  const c = crypto.createCipheriv('chacha20-poly1305', k, nonce(n), { authTagLength: 16 });
  c.setAAD(ad);
  return Buffer.concat([c.update(pt), c.final(), c.getAuthTag()]);
}
function aeadDec(k, n, ad, ct) {
  const c = crypto.createDecipheriv('chacha20-poly1305', k, nonce(n), { authTagLength: 16 });
  c.setAAD(ad);
  c.setAuthTag(ct.subarray(ct.length - 16));
  return Buffer.concat([c.update(ct.subarray(0, ct.length - 16)), c.final()]);
}

class Symmetric {
  constructor(name) {
    const nb = Buffer.from(name);
    this.h = nb.length <= 32 ? Buffer.concat([nb, Buffer.alloc(32 - nb.length)]) : sha256(nb);
    this.ck = Buffer.from(this.h);
    this.k = null;
    this.n = 0;
  }
  mixHash(d) { this.h = sha256(Buffer.concat([this.h, d])); }
  mixKey(ikm) { const [ck, k] = hkdf(this.ck, ikm, 2); this.ck = ck; this.k = k.subarray(0, 32); this.n = 0; }
  mixKeyAndHash(ikm) { const [ck, h2, k] = hkdf(this.ck, ikm, 3); this.ck = ck; this.mixHash(h2); this.k = k.subarray(0, 32); this.n = 0; }
  encryptAndHash(pt) { const ct = this.k ? (() => { const c = aeadEnc(this.k, this.n, this.h, pt); this.n++; return c; })() : pt; this.mixHash(ct); return ct; }
  decryptAndHash(ct) { const pt = this.k ? (() => { const p = aeadDec(this.k, this.n, this.h, ct); this.n++; return p; })() : ct; this.mixHash(ct); return pt; }
  split() { const [t1, t2] = hkdf(this.ck, Buffer.alloc(0), 2); return [new Cipher(t1.subarray(0, 32)), new Cipher(t2.subarray(0, 32))]; }
  hasKey() { return this.k != null; }
}

class Cipher {
  constructor(k) { this.k = k; this.n = 0; }
  encrypt(pt) { const c = aeadEnc(this.k, this.n, Buffer.alloc(0), pt); this.n++; return c; }
  decrypt(ct) { const p = aeadDec(this.k, this.n, Buffer.alloc(0), ct); this.n++; return p; }
}

// XXpsk3 initiator. Because a PSK is used, every `e` token also MixKey's.
export class Initiator {
  constructor(staticPriv, staticPub, psk) {
    this.s = { priv: staticPriv, pub: staticPub };
    this.psk = psk;
    this.sym = new Symmetric(NAME);
    this.sym.mixHash(Buffer.alloc(0)); // empty prologue
    this.done = false;
  }
  // -> e
  writeMsg1() {
    this.e = genKeypair();
    this.sym.mixHash(this.e.pub);
    this.sym.mixKey(this.e.pub);
    return Buffer.concat([this.e.pub, this.sym.encryptAndHash(Buffer.alloc(0))]);
  }
  // <- e, ee, s, es
  readMsg2(msg) {
    let i = 0;
    this.re = msg.subarray(i, i + 32); i += 32;
    this.sym.mixHash(this.re);
    this.sym.mixKey(this.re);
    this.sym.mixKey(dh(this.e.priv, this.re)); // ee
    const slen = this.sym.hasKey() ? 48 : 32;
    const sct = msg.subarray(i, i + slen); i += slen;
    this.rs = this.sym.decryptAndHash(sct);    // remote static (controller pubkey)
    this.sym.mixKey(dh(this.e.priv, this.rs)); // es
    this.sym.decryptAndHash(msg.subarray(i));  // payload (empty)
  }
  // -> s, se, psk
  writeMsg3() {
    const sct = this.sym.encryptAndHash(this.s.pub);
    this.sym.mixKey(dh(this.s.priv, this.re)); // se
    this.sym.mixKeyAndHash(this.psk);          // psk
    const payload = this.sym.encryptAndHash(Buffer.alloc(0));
    const [send, recv] = this.sym.split();
    this.send = send; this.recv = recv; this.done = true;
    return Buffer.concat([sct, payload]);
  }
  remoteStatic() { return this.rs; }            // controller's static pubkey (pin this)
  encrypt(pt) { return this.send.encrypt(pt); }
  decrypt(ct) { return this.recv.decrypt(ct); }
}

// ── Key derivations (match Rust e2ee.rs) ─────────────────────────────────────
export const derivePsk = (pairing) => hmac(Buffer.from('claude-pty-controller/psk/v1'), Buffer.from(pairing));
const deriveRendezvous = (pairing) => hmac(Buffer.from('claude-pty-controller/rendezvous/v1'), Buffer.from(pairing));

export function roomId(pairing, epoch) {
  const rs = deriveRendezvous(pairing);
  const e = Buffer.alloc(8); e.writeBigUInt64BE(BigInt(epoch));
  const mac = crypto.createHmac('sha256', rs).update(Buffer.from('rendezvous')).update(e).digest();
  return mac.subarray(0, 16).toString('hex');
}
export const EPOCH_WINDOW = 300;
export function roomIdNow(pairing, nowSecs = Math.floor(Date.now() / 1000)) {
  return roomId(pairing, Math.floor(nowSecs / EPOCH_WINDOW));
}
