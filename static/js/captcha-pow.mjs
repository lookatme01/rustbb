// Proof-of-work for the captcha forms (Admin CP > Settings > Security > captchapow).
//
// The board hands every captcha a random hash. The browser must find a number n such that
// SHA-256("<hash>:<n>") starts with `bits` zero bits, and sends it appended to the typed answer
// ("123456~n"). A person's browser does it in a fraction of a second; a script registering
// thousands of accounts pays that CPU time for each one. It adds to the image captcha, it does
// not replace it, and it does not stop an attacker willing to spend the CPU.
//
// SHA-256 is written out here because the message is always shorter than one 64-byte block, which
// makes a synchronous implementation far faster than awaiting crypto.subtle for every try.

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

const W = new Uint32Array(64);
const block = new Uint8Array(64);

/** SHA-256 of an ASCII message of at most 55 bytes, as eight 32-bit words. */
export function sha256Words(msg) {
  const n = msg.length;
  if (n > 55) throw new Error("message too long for a single block");
  block.fill(0);
  for (let i = 0; i < n; i++) block[i] = msg.charCodeAt(i);
  block[n] = 0x80;
  const bitLen = n * 8;
  block[62] = (bitLen >>> 8) & 0xff;
  block[63] = bitLen & 0xff;
  for (let i = 0; i < 16; i++) {
    W[i] = (block[i * 4] << 24) | (block[i * 4 + 1] << 16) | (block[i * 4 + 2] << 8) | block[i * 4 + 3];
  }
  for (let i = 16; i < 64; i++) {
    const a = W[i - 15], b = W[i - 2];
    const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
    const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
    W[i] = (W[i - 16] + s0 + W[i - 7] + s1) | 0;
  }
  let h0 = 0x6a09e667, h1 = 0xbb67ae85, h2 = 0x3c6ef372, h3 = 0xa54ff53a;
  let h4 = 0x510e527f, h5 = 0x9b05688c, h6 = 0x1f83d9ab, h7 = 0x5be0cd19;
  let a = h0, b = h1, c = h2, d = h3, e = h4, f = h5, g = h6, h = h7;
  for (let i = 0; i < 64; i++) {
    const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
    const ch = (e & f) ^ (~e & g);
    const t1 = (h + S1 + ch + K[i] + W[i]) | 0;
    const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
    const maj = (a & b) ^ (a & c) ^ (b & c);
    const t2 = (S0 + maj) | 0;
    h = g; g = f; f = e; e = (d + t1) | 0; d = c; c = b; b = a; a = (t1 + t2) | 0;
  }
  return [(h0 + a) | 0, (h1 + b) | 0, (h2 + c) | 0, (h3 + d) | 0, (h4 + e) | 0, (h5 + f) | 0, (h6 + g) | 0, (h7 + h) | 0];
}

/** Whether SHA-256("<challenge>:<nonce>") starts with at least `bits` zero bits (bits <= 32). */
export function meetsTarget(challenge, nonce, bits) {
  return Math.clz32(sha256Words(`${challenge}:${nonce}`)[0]) >= bits;
}

/**
 * Find a nonce for `challenge`. Works in slices so the page stays responsive; `onTick` is called
 * between them (the tests pass none and get a plain loop).
 */
export async function solve(challenge, bits, onTick) {
  let nonce = Math.floor(Math.random() * 1e6);
  for (;;) {
    for (let i = 0; i < 20000; i++, nonce++) {
      if (meetsTarget(challenge, nonce, bits)) return String(nonce);
    }
    if (onTick) await onTick();
  }
}

// In a page: solve the challenge when a captcha form is submitted, then submit it for real.
if (typeof document !== "undefined" && document.addEventListener) {
  document.addEventListener(
    "submit",
    async (e) => {
      const form = e.target;
      const input = form.querySelector && form.querySelector("[data-pow-bits]");
      const hashField = form.querySelector && form.querySelector(".js-captcha-hash");
      if (!input || !hashField) return;
      const challenge = hashField.value;
      if (input.dataset.powFor === challenge) return; // already solved for this image
      e.preventDefault();
      const button = e.submitter || form.querySelector("[type=submit]");
      const label = button ? button.textContent : "";
      if (button) {
        button.disabled = true;
        button.textContent = "Checking your browser…";
      }
      const nonce = await solve(challenge, Number(input.dataset.powBits), () => new Promise((r) => setTimeout(r, 0)));
      input.value = `${input.value.split("~")[0]}~${nonce}`;
      input.dataset.powFor = challenge;
      if (button) {
        button.disabled = false;
        button.textContent = label;
      }
      if (form.requestSubmit) form.requestSubmit(e.submitter || undefined);
      else form.submit();
    },
    true,
  );
}
