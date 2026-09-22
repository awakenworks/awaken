import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { X509Certificate } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { createTlsIdentityFixture, isTlsIdentityUsable } from './tls_identity_fixture.mjs';

test('local TLS fixture defaults to one day and admits only an explicit bounded longer life', () => {
  /* Certificate-fixture decision table. C1 no validity option, C2 an explicit
   * integer 1..30, C3 noninteger/out-of-range, C4 an invalid SAN. E1 retain
   * the existing one-day test identity; E2 issue CA and leaf for the same
   * requested interval; E3 reject before producing an identity. Rules
   * F1=C1=>E1, F2=C2=>E2, F3=C3|C4=>E3. This is a fixture option, never a
   * second product trust or certificate-validation authority. */
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-tls-validity-'));
  try {
    const input = { caCommonName: 'Fixture CA', serverCommonName: 'awaken-product',
      subjectAltNames: ['DNS:awaken-product'] };
    for (const [label, days] of [['default', undefined], ['persistent-local', 30]]) {
      const { caCertificate, serverCertificate } = createTlsIdentityFixture(path.join(root, label), {
        ...input, ...(days === undefined ? {} : { validityDays: days }),
      });
      for (const file of [caCertificate, serverCertificate]) {
        const cert = new X509Certificate(fs.readFileSync(file));
        const durationDays = (Date.parse(cert.validTo) - Date.parse(cert.validFrom)) / 86_400_000;
        assert.equal(durationDays, days ?? 1, label);
      }
    }
    for (const validityDays of [0, 31, 1.5, Number.NaN]) {
      assert.throws(() => createTlsIdentityFixture(path.join(root, `bad-${validityDays}`), {
        ...input, validityDays,
      }), /validityDays/);
    }
    assert.throws(() => createTlsIdentityFixture(path.join(root, 'bad-san'), {
      ...input, subjectAltNames: ['DNS:wrong/name'], validityDays: 30,
    }), /subjectAltName/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('local TLS reuse requires exact CA, host, key and remaining validity', () => {
  /* Reuse decision table. C1 CA+leaf+key+host match and have >7 days left;
   * C2 current time reaches the reserve; C3 wrong host/CA/key; C4 malformed
   * certificate. E1 only C1 may reuse; E2 C2|C3|C4 must rotate. Rules
   * V1=C1=>E1, V2=C2|C3|C4=>E2. Time is injected for boundary tests; no wall
   * clock sleep or second certificate generator is involved. */
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-tls-reuse-'));
  try {
    const make = (name) => createTlsIdentityFixture(path.join(root, name), {
      caCommonName: `CA ${name}`, serverCommonName: 'awaken-product',
      subjectAltNames: ['DNS:awaken-product'], validityDays: 30,
    });
    const first = make('first');
    const second = make('second');
    const bytes = (file) => fs.readFileSync(file);
    const identity = {
      caCertificate: bytes(first.caCertificate),
      serverCertificate: bytes(first.serverCertificate),
      serverKey: bytes(first.serverKey), host: 'awaken-product',
    };
    const leaf = new X509Certificate(identity.serverCertificate);
    const sevenDays = 7 * 86_400_000;
    const issued = Date.parse(leaf.validFrom);
    const expires = Date.parse(leaf.validTo);
    assert.equal(isTlsIdentityUsable(identity, { now: issued + 60_000,
      minRemainingMs: sevenDays }), true, 'V1');
    assert.equal(isTlsIdentityUsable(identity, { now: expires - sevenDays,
      minRemainingMs: sevenDays }), false, 'V2 reserve boundary');
    assert.equal(isTlsIdentityUsable({ ...identity, host: 'wrong' }, { now: issued + 60_000 }), false, 'V2 host');
    assert.equal(isTlsIdentityUsable({ ...identity, caCertificate: bytes(second.caCertificate) },
      { now: issued + 60_000 }), false, 'V2 CA');
    assert.equal(isTlsIdentityUsable({ ...identity, serverKey: bytes(second.serverKey) },
      { now: issued + 60_000 }), false, 'V2 key');
    assert.equal(isTlsIdentityUsable({ ...identity, serverCertificate: Buffer.from('broken') }), false, 'V2 malformed');
    const verify = (...args) => spawnSync(process.execPath, [new URL('./tls_identity_fixture.mjs', import.meta.url).pathname,
      'verify', first.caCertificate, first.serverCertificate, first.serverKey, ...args]);
    assert.equal(verify('awaken-product', '604800').status, 0, 'V1 CLI');
    assert.equal(verify('wrong', '604800').status, 1, 'V2 CLI host');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
