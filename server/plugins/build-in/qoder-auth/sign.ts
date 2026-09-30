/**
 * COSY signing for Qoder's /algo endpoints.
 *
 * Qoder's CLI gateway (api1/api2/api3.qoder.sh) rejects unsigned requests with
 * "Signature invalid". The scheme, ported from the community reference
 * implementations of the official client, is:
 *
 *   tempKey = 16 random ASCII characters
 *   cosyKey = base64(RSA-PKCS1v1.5(tempKey))        // server RSA public key
 *   info    = base64(AES-128-CBC(key = iv = tempKey, sorted-compact(identity)))
 *   payload = base64(sorted-compact({cosyVersion, ideVersion, info, requestId, version}))
 *   sig     = md5(payload + "\n" + cosyKey + "\n" + date + "\n" + body + "\n" + path)
 *   Authorization = "Bearer COSY." + payload + "." + sig
 *
 * `path` is the request path without the "/algo" prefix, and `body` is the
 * custom-base64 encoded request body — the exact bytes that are sent, so a GET
 * must carry the same body it signed.
 */
import type { JsonValue } from "cursor-byok:plugin";

const COSY_VERSION = "1.1.64";
const CUSTOM_ALPHABET = "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const STANDARD_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const CUSTOM_PAD = "$";

const TO_CUSTOM = new Map<string, string>();
const TO_STANDARD = new Map<string, string>();
for (let index = 0; index < 64; index++) {
  TO_CUSTOM.set(STANDARD_ALPHABET[index], CUSTOM_ALPHABET[index]);
  TO_STANDARD.set(CUSTOM_ALPHABET[index], STANDARD_ALPHABET[index]);
}
TO_CUSTOM.set("=", CUSTOM_PAD);
TO_STANDARD.set(CUSTOM_PAD, "=");

// Qoder's server public key (SPKI PEM, 1024-bit) and the RSA exponent.
const RSA_N = BigInt(
  "0xc0f22307e5cd362e296bb04470f6de8fbf935ce24e8fcf511a0e2701329769c4a76e499bb938036a52af1eaf818cf79a2600620e3ce87e371d2ca6d8580360" +
    "6a1b3fa5e874643c9ed2db7e85673ef7227fca56e2e7c08f0927609bb896a9f24be1782099a66016a5bfdc3f1ff756bfc9e88d7b5dc5be30bf45a0223a00ebcecf",
);
const RSA_E = 65537n;

const encoder = new TextEncoder();

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

function fromBase64(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index++) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

function concat(parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((sum, part) => sum + part.length, 0);
  const merged = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    merged.set(part, offset);
    offset += part.length;
  }
  return merged;
}

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** Qoder's custom base64 variant used for request bodies. */
export function qoderEncode(plain: Uint8Array | string): string {
  const bytes = typeof plain === "string" ? encoder.encode(plain) : plain;
  const standard = base64(bytes);
  const rotate = Math.floor(standard.length / 3);
  const rearranged = standard.slice(standard.length - rotate) +
    standard.slice(rotate, standard.length - rotate) + standard.slice(0, rotate);
  let encoded = "";
  for (const character of rearranged) {
    const mapped = TO_CUSTOM.get(character);
    if (mapped === undefined) throw new Error(`custom base64 alphabet is missing '${character}'`);
    encoded += mapped;
  }
  return encoded;
}

/** Inverse of `qoderEncode`; used by tests and diagnostics. */
export function qoderDecode(encoded: string): Uint8Array {
  let standard = "";
  for (const character of encoded) {
    const mapped = TO_STANDARD.get(character);
    if (mapped === undefined) throw new Error(`custom base64 alphabet is missing '${character}'`);
    standard += mapped;
  }
  const rotate = Math.floor(standard.length / 3);
  const head = standard.slice(0, rotate);
  const middle = standard.slice(rotate, standard.length - rotate);
  const tail = standard.slice(standard.length - rotate);
  return fromBase64(tail + middle + head);
}

/** MD5 over bytes or UTF-8 text, lowercase hex. WebCrypto has no MD5. */
export function md5Hex(input: Uint8Array | string): string {
  const message = typeof input === "string" ? encoder.encode(input) : input;
  const padded = new Uint8Array((((message.length + 8) >> 6) + 1) << 6);
  padded.set(message);
  padded[message.length] = 0x80;
  const view = new DataView(padded.buffer);
  const bits = message.length * 8;
  view.setUint32(padded.length - 8, bits >>> 0, true);
  view.setUint32(padded.length - 4, Math.floor(bits / 0x1_0000_0000), true);

  const shifts = [
    7,
    12,
    17,
    22,
    7,
    12,
    17,
    22,
    7,
    12,
    17,
    22,
    7,
    12,
    17,
    22,
    5,
    9,
    14,
    20,
    5,
    9,
    14,
    20,
    5,
    9,
    14,
    20,
    5,
    9,
    14,
    20,
    4,
    11,
    16,
    23,
    4,
    11,
    16,
    23,
    4,
    11,
    16,
    23,
    4,
    11,
    16,
    23,
    6,
    10,
    15,
    21,
    6,
    10,
    15,
    21,
    6,
    10,
    15,
    21,
    6,
    10,
    15,
    21,
  ];
  const constants = new Uint32Array(64);
  for (let index = 0; index < 64; index++) {
    constants[index] = Math.floor(Math.abs(Math.sin(index + 1)) * 0x1_0000_0000);
  }

  let a0 = 0x67452301;
  let b0 = 0xefcdab89;
  let c0 = 0x98badcfe;
  let d0 = 0x10325476;

  for (let offset = 0; offset < padded.length; offset += 64) {
    const words = new Uint32Array(16);
    for (let index = 0; index < 16; index++) {
      words[index] = view.getUint32(offset + index * 4, true);
    }
    let a = a0;
    let b = b0;
    let c = c0;
    let d = d0;
    for (let index = 0; index < 64; index++) {
      let mixed: number;
      let word: number;
      if (index < 16) {
        mixed = (b & c) | (~b & d);
        word = index;
      } else if (index < 32) {
        mixed = (d & b) | (~d & c);
        word = (5 * index + 1) % 16;
      } else if (index < 48) {
        mixed = b ^ c ^ d;
        word = (3 * index + 5) % 16;
      } else {
        mixed = c ^ (b | ~d);
        word = (7 * index) % 16;
      }
      const sum = (a + mixed + constants[index] + words[word]) >>> 0;
      const shift = shifts[index];
      const rotated = ((sum << shift) | (sum >>> (32 - shift))) >>> 0;
      a = d;
      d = c;
      c = b;
      b = (b + rotated) >>> 0;
    }
    a0 = (a0 + a) >>> 0;
    b0 = (b0 + b) >>> 0;
    c0 = (c0 + c) >>> 0;
    d0 = (d0 + d) >>> 0;
  }

  const digest = new Uint8Array(16);
  const digestView = new DataView(digest.buffer);
  digestView.setUint32(0, a0, true);
  digestView.setUint32(4, b0, true);
  digestView.setUint32(8, c0, true);
  digestView.setUint32(12, d0, true);
  return toHex(digest);
}

function modPow(base: bigint, exponent: bigint, modulus: bigint): bigint {
  let result = 1n;
  let value = base % modulus;
  let power = exponent;
  while (power > 0n) {
    if (power & 1n) result = (result * value) % modulus;
    value = (value * value) % modulus;
    power >>= 1n;
  }
  return result;
}

/** RSA PKCS#1 v1.5 (type 2) public-key encryption. `padding` is for tests. */
export function rsaPkcs1v15Encrypt(plain: Uint8Array, padding?: Uint8Array): Uint8Array {
  const size = 128;
  if (plain.length > size - 11) throw new Error("RSA block is too short for the message");
  const paddingLength = size - 3 - plain.length;
  let ps = padding;
  if (ps === undefined) {
    ps = new Uint8Array(paddingLength);
    let filled = 0;
    while (filled < paddingLength) {
      for (const byte of crypto.getRandomValues(new Uint8Array(paddingLength - filled))) {
        if (byte === 0) continue;
        ps[filled++] = byte;
        if (filled === paddingLength) break;
      }
    }
  }
  if (ps.length !== paddingLength) throw new Error("RSA padding length mismatch");
  const block = concat([new Uint8Array([0x00, 0x02]), ps, new Uint8Array([0x00]), plain]);
  const cipher = modPow(BigInt(`0x${toHex(block)}`), RSA_E, RSA_N);
  const hex = cipher.toString(16).padStart(size * 2, "0");
  const out = new Uint8Array(size);
  for (let index = 0; index < size; index++) {
    out[index] = Number.parseInt(hex.slice(index * 2, index * 2 + 2), 16);
  }
  return out;
}

/** AES-128-CBC with PKCS#7 padding; Qoder uses the same bytes as key and IV. */
export async function aesCbcEncrypt(
  plain: Uint8Array<ArrayBuffer>,
  key: Uint8Array<ArrayBuffer>,
): Promise<Uint8Array> {
  const cryptoKey = await crypto.subtle.importKey("raw", key, { name: "AES-CBC" }, false, [
    "encrypt",
  ]);
  // WebCrypto's AES-CBC pads with PKCS#7 itself, exactly like the reference.
  const sealed = await crypto.subtle.encrypt({ name: "AES-CBC", iv: key }, cryptoKey, plain);
  return new Uint8Array(sealed);
}

/** Sorted-key compact JSON over a flat map of strings, as the signature expects. */
export function jsonSortedCompact(record: Record<string, JsonValue>): string {
  return `{${
    Object.keys(record).sort().map((key) =>
      `${JSON.stringify(key)}:${JSON.stringify(record[key] ?? "")}`
    ).join(",")
  }}`;
}

function deriveId(uid: string, salt: string): string {
  return md5Hex(`${salt}:${uid || "anonymous"}`).slice(0, 36);
}

/** Stable pseudo-device fingerprint derived from the account uid. */
export async function machineIdentity(
  uid: string,
): Promise<{ machineId: string; machineToken: string; machineType: string }> {
  const digest = await crypto.subtle.digest("SHA-512", encoder.encode(`machinetoken:${uid}`));
  const token = btoa(String.fromCharCode(...new Uint8Array(digest)))
    .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "").slice(0, 43);
  return {
    machineId: deriveId(uid, "machine"),
    machineToken: token,
    machineType: deriveId(uid, "machinetype").replace(/-/g, "").slice(0, 18),
  };
}

export type CosyIdentity = {
  uid: string;
  name: string;
  organizationId: string;
  organizationName: string;
  userType: string;
  accessToken: string;
  refreshToken: string;
};

type SignedRequest = {
  headers: Record<string, string>;
  body: string;
};

type CosySession = {
  sign(body: string, rawUrl: string): SignedRequest;
};

const IDENTITY_KEYS = [
  "name",
  "aid",
  "uid",
  "yx_uid",
  "organization_id",
  "organization_name",
  "user_type",
  "security_oauth_token",
  "refresh_token",
];

export const DEFAULT_USER_TYPE = "personal_professional_trial";

/** Per-account signing session; rebuilt whenever the access token rotates. */
async function createCosySession(identity: CosyIdentity): Promise<CosySession> {
  const tempKey = encoder.encode(crypto.randomUUID().replace(/-/g, "").slice(0, 16));
  const cosyKey = base64(rsaPkcs1v15Encrypt(tempKey));
  const values = [
    identity.name,
    identity.uid,
    identity.uid,
    "",
    identity.organizationId,
    identity.organizationName,
    identity.userType || DEFAULT_USER_TYPE,
    identity.accessToken,
    identity.refreshToken,
  ];
  const info = base64(
    await aesCbcEncrypt(
      encoder.encode(
        jsonSortedCompact(
          Object.fromEntries(IDENTITY_KEYS.map((key, index) => [key, values[index]])),
        ),
      ),
      tempKey,
    ),
  );
  const machines = await machineIdentity(identity.uid);

  return {
    sign(body: string, rawUrl: string): SignedRequest {
      const url = new URL(rawUrl);
      const path = url.pathname.startsWith("/algo")
        ? url.pathname.slice("/algo".length)
        : url.pathname;
      const date = String(Math.floor(Date.now() / 1000));
      const payload = base64(encoder.encode(jsonSortedCompact({
        cosyVersion: COSY_VERSION,
        ideVersion: "",
        info,
        requestId: crypto.randomUUID(),
        version: "v1",
      })));
      const signature = md5Hex(`${payload}\n${cosyKey}\n${date}\n${body}\n${path}`);
      return {
        body,
        headers: {
          "cosy-data-policy": "AGREE",
          "content-type": "application/json",
          "cosy-machinetype": machines.machineType,
          "cosy-clienttype": "5",
          "cosy-date": date,
          "cosy-user": identity.uid,
          "cosy-key": cosyKey,
          "cosy-clientip": "169.254.198.161",
          "authorization": `Bearer COSY.${payload}.${signature}`,
          "accept-encoding": "identity",
          "cosy-version": COSY_VERSION,
          "cosy-machineid": machines.machineId,
          "cosy-machinetoken": machines.machineToken,
          "login-version": "v2",
          "user-agent": "Go-http-client/2.0",
        },
      };
    },
  };
}

const sessions = new Map<string, Promise<CosySession>>();
const SESSION_CACHE_SIZE = 64;

/** Coalesce session creation per account and drop it when the token rotates. */
export function cosySession(identity: CosyIdentity): Promise<CosySession> {
  const key = `${identity.uid}\u0000${identity.accessToken}`;
  const existing = sessions.get(key);
  if (existing) return existing;
  const created = createCosySession(identity);
  sessions.set(key, created);
  if (sessions.size > SESSION_CACHE_SIZE) {
    const oldest = sessions.keys().next().value;
    if (oldest !== undefined) sessions.delete(oldest);
  }
  return created;
}
