import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/**
 * Throwaway TLS material for tests, generated with the `openssl` CLI at test
 * time. Nothing is committed; keys live in a temp dir removed by `dispose()`.
 */
export interface TestPki {
  /** PEM of the throwaway root CA (pass as FetchPolicy.ca). */
  readonly caCert: string;
  /** Issue a CA-signed server certificate for a subjectAltName list, e.g. `IP:127.0.0.1,DNS:localhost`. */
  issue(san: string): { cert: string; key: string };
  /** Delete the temp directory holding the CA key. */
  dispose(): void;
}

/** True when an `openssl` binary is on PATH. */
export function opensslAvailable(): boolean {
  try {
    execFileSync('openssl', ['version'], { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

/** Reason string for `t.skip` / `{ skip }` when openssl is missing. */
export const SKIP_REASON = 'openssl not found on PATH; TLS fixture tests need it to generate a throwaway CA';

/** Create a throwaway CA and return an issuer. Throws if openssl is unusable. */
export function createTestPki(): TestPki {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sitefile-pki-'));
  const run = (args: string[]): void => {
    execFileSync('openssl', args, { cwd: dir, stdio: 'pipe' });
  };
  try {
    run([
      'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'ca.key', '-out', 'ca.pem',
      '-days', '2', '-subj', '/CN=Sitefile Test CA',
      '-addext', 'basicConstraints=critical,CA:TRUE',
      '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
    ]);
  } catch (e) {
    rmSync(dir, { recursive: true, force: true });
    throw e;
  }
  const caCert = readFileSync(path.join(dir, 'ca.pem'), 'utf8');
  let n = 0;
  return {
    caCert,
    issue(san) {
      const id = `srv${n++}`;
      writeFileSync(
        path.join(dir, `${id}.ext`),
        `subjectAltName=${san}\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n`,
      );
      run(['req', '-newkey', 'rsa:2048', '-nodes', '-keyout', `${id}.key`, '-out', `${id}.csr`, '-subj', '/CN=sitefile-test-server']);
      run([
        'x509', '-req', '-in', `${id}.csr`, '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial',
        '-out', `${id}.pem`, '-days', '2', '-extfile', `${id}.ext`,
      ]);
      return {
        cert: readFileSync(path.join(dir, `${id}.pem`), 'utf8'),
        key: readFileSync(path.join(dir, `${id}.key`), 'utf8'),
      };
    },
    dispose() {
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
