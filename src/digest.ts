import { createHash } from 'node:crypto';

/** `sha256:<hex>` digest of bytes or a string. */
export function sha256(data: Uint8Array | string): string {
  return 'sha256:' + createHash('sha256').update(data).digest('hex');
}

export const INTEGRITY_RE = /^sha256:[0-9a-f]{64}$/;
