import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseDurableJson, stringifyDurableJson } from './sqlite.mjs';

// Durable-JSON cause/effect graph: C1 an integer token is safe; C2 an integer
// token exceeds Number's exact range; C3 the same digits occur inside a string;
// C4 a numeric token uses decimal/exponent notation; C5 values are nested.
// Effects: E1 ordinary JSON types are unchanged; E2 unsafe integer bytes survive
// a read/edit/write cycle; E3 strings and non-integer notation are not recast.
// Decision rules: C1=>E1; C2=>E2; C3||C4=>E3; any rule+C5=>same effect.
test('round-trips durable unsafe integers without changing other JSON values', () => {
  const source = '{"safe":42,"lease":18446744073709551615,"negative":-9007199254740993,"label":"18446744073709551615","decimal":1.5,"exponent":1e+21,"nested":[18446744073709551614]}';
  const parsed = parseDurableJson(source);

  assert.equal(parsed.safe, 42, 'C1 => E1');
  assert.equal(parsed.lease, 18446744073709551615n, 'C2 => E2');
  assert.equal(parsed.negative, -9007199254740993n, 'negative C2 => E2');
  assert.equal(parsed.label, '18446744073709551615', 'C3 => E3');
  assert.equal(parsed.decimal, 1.5, 'decimal C4 => E3');
  assert.equal(parsed.exponent, 1e21, 'exponent C4 => E3');
  assert.equal(parsed.nested[0], 18446744073709551614n, 'C2+C5 => E2');

  parsed.safe = 43;
  const encoded = stringifyDurableJson(parsed);
  assert.match(encoded, /"safe":43/);
  assert.match(encoded, /"lease":18446744073709551615/);
  assert.match(encoded, /"negative":-9007199254740993/);
  assert.match(encoded, /"label":"18446744073709551615"/);
  assert.match(encoded, /"nested":\[18446744073709551614\]/);
  assert.deepEqual(parseDurableJson(encoded), parsed);
});
