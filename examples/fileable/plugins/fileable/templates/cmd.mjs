// Shows what the sandbox refuses: fileable's cmd attribute shells out, and a
// Sitefile plugin cannot start processes. Building with
// --build-arg TEMPLATE=cmd.mjs fails with E_PLUGIN and says so.
import { file } from '@johnhenry/fileable/browser';

export default function cmd() {
  return file('version.txt', null, { cmd: 'git describe --tags' });
}
