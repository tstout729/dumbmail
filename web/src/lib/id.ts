const alphabet = "abcdefghijkmnpqrstuvwxyz23456789";

/** Short random id for flow nodes and other client-generated keys. */
export function shortId(length = 8): string {
  const bytes = crypto.getRandomValues(new Uint8Array(length));
  let out = "";
  for (const b of bytes) out += alphabet[b % alphabet.length];
  return out;
}
