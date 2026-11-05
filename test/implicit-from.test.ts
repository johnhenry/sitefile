import { test } from 'node:test';
import assert from 'node:assert/strict';
import { plan } from '../src/planner.js';
import { parseProgram } from '../src/language/validate.js';

const planOf = (src: string, args: Record<string, string> = {}) => plan(parseProgram(src, 'Sitefile'), args);

test('a Sitefile with no FROM plans one unnamed scratch stage', () => {
  const p = planOf('COPY ./a /a\nEXPORT /');
  assert.equal(p.stages.length, 1);
  assert.deepEqual(p.stages[0]!.base, { kind: 'scratch' });
  assert.equal(p.stages[0]!.name, undefined);
  assert.equal(p.stages[0]!.operations.length, 1);
  assert.deepEqual(p.warnings, []);
});

test('global ARGs before the implicit stage are visible in it', () => {
  const p = planOf('ARG T = "x"\nLET U = "${T}!"\nEXPORT /', { T: 'y' });
  assert.equal(p.vars.get('U'), 'y!');
  assert.equal(p.stages.length, 1);
  assert.equal(p.stages[0]!.span.line, 2);
});

test('implicit stage followed by named stages', () => {
  const p = planOf('COPY ./a /a\nFROM scratch AS two\nCOPY ./b /b\nEXPORT /');
  assert.equal(p.stages.length, 2);
  // The implicit stage is unnamed and not last: warn, at the first instruction.
  assert.deepEqual(p.warnings, ['Sitefile:1: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>']);
});

test('unnamed non-final stages warn; named and final ones do not', () => {
  assert.deepEqual(planOf('FROM scratch\nEXPORT /').warnings, []);
  assert.deepEqual(planOf('FROM scratch AS a\nFROM a\nEXPORT /').warnings, []);
  const w = planOf('FROM scratch\nFROM scratch\nFROM scratch AS c\nFROM c\nEXPORT /', { Z: '1' }).warnings;
  assert.equal(w.length, 3);
  assert.match(w[0]!, /^Sitefile:1: unnamed stage/);
  assert.match(w[1]!, /^Sitefile:2: unnamed stage/);
  assert.match(w[2]!, /--build-arg Z/);
});
