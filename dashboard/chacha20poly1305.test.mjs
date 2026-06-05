// node --test chacha20poly1305.test.mjs
// Verifies the vendored AEAD matches node:crypto byte-for-byte.
import { test } from 'node:test';
import assert from 'node:assert';
import crypto from 'node:crypto';
import { seal, open } from './chacha20poly1305.mjs';

function nodeSeal(key, nonce, aad, pt) {
  const c = crypto.createCipheriv('chacha20-poly1305', key, nonce, { authTagLength: 16 });
  c.setAAD(aad);
  return Buffer.concat([c.update(pt), c.final(), c.getAuthTag()]);
}

test('seal matches node:crypto and open round-trips', () => {
  for (const [ptlen, aadlen] of [[0, 0], [1, 0], [16, 5], [63, 32], [64, 0], [65, 16], [200, 3], [1000, 64]]) {
    const key = crypto.randomBytes(32);
    const nonce = crypto.randomBytes(12);
    const aad = crypto.randomBytes(aadlen);
    const pt = crypto.randomBytes(ptlen);
    const mine = Buffer.from(seal(key, nonce, aad, pt));
    const theirs = nodeSeal(key, nonce, aad, pt);
    assert.ok(mine.equals(theirs), `seal mismatch ptlen=${ptlen} aadlen=${aadlen}`);
    assert.ok(Buffer.from(open(key, nonce, aad, theirs)).equals(pt), `open mismatch ptlen=${ptlen}`);
  }
});

test('open rejects tampered ciphertext', () => {
  const key = crypto.randomBytes(32), nonce = crypto.randomBytes(12);
  const ct = seal(key, nonce, new Uint8Array(0), Buffer.from('secret'));
  ct[0] ^= 1;
  assert.throws(() => open(key, nonce, new Uint8Array(0), ct));
});
