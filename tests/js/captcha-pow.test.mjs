// The captcha proof-of-work: `node --test tests/js`.
//
// The page-side SHA-256 is hand-written, so it is checked against Node's own against many inputs,
// and solved nonces are verified with the library, not with the code under test.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { sha256Words, meetsTarget, solve } from "../../static/js/captcha-pow.mjs";

const words = (msg) => {
  const d = createHash("sha256").update(msg).digest();
  return [0, 1, 2, 3, 4, 5, 6, 7].map((i) => d.readInt32BE(i * 4));
};

const leadingZeroBits = (msg) => {
  const d = createHash("sha256").update(msg).digest();
  let bits = 0;
  for (const byte of d) {
    if (byte === 0) { bits += 8; continue; }
    bits += Math.clz32(byte) - 24;
    break;
  }
  return bits;
};

test("sha256 matches node's for every length a challenge can have", () => {
  for (let n = 0; n <= 55; n++) {
    const msg = randomBytes(n).toString("hex").slice(0, n).padEnd(n, "x");
    assert.deepEqual(sha256Words(msg), words(msg), `length ${n}`);
  }
  assert.deepEqual(sha256Words("abc"), words("abc"));
  assert.deepEqual(sha256Words(""), words(""));
});

test("messages longer than one block are refused, not hashed wrongly", () => {
  assert.throws(() => sha256Words("x".repeat(56)));
});

test("solved nonces meet the target when checked independently", async () => {
  for (const bits of [4, 10, 14]) {
    const challenge = randomBytes(16).toString("hex");
    const nonce = await solve(challenge, bits);
    assert.match(nonce, /^\d{1,12}$/, "the server only accepts a plain number");
    assert.ok(leadingZeroBits(`${challenge}:${nonce}`) >= bits, `${bits} bits`);
    assert.ok(meetsTarget(challenge, nonce, bits));
  }
});

test("a wrong nonce usually fails a hard target", () => {
  const challenge = randomBytes(16).toString("hex");
  let passes = 0;
  for (let n = 0; n < 2000; n++) if (meetsTarget(challenge, n, 20)) passes++;
  assert.ok(passes < 5, `${passes} of 2000 nonces passed a 20-bit target`);
});
