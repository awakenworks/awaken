import { spawnSync } from 'node:child_process';
import { X509Certificate, createPrivateKey } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

function runOpenSsl(args, purpose) {
  const result = spawnSync('openssl', args, { encoding: 'utf8' });
  if (result.status !== 0) {
    throw new Error(
      `${purpose}: ${result.stderr || result.stdout || `openssl exited ${result.status}`}`,
    );
  }
}

function requireIdentityPart(value, label) {
  if (typeof value !== 'string' || value.trim() === '' || /[\r\n/]/u.test(value)) {
    throw new Error(`${label} must be one non-empty certificate subject component`);
  }
  return value;
}

function requireSubjectAltName(value) {
  if (!/^(?:DNS:[A-Za-z0-9.-]+|IP:[0-9A-Fa-f:.]+)$/u.test(value)) {
    throw new Error(`invalid explicit TLS fixture subjectAltName: ${value}`);
  }
  return value;
}

// This is the sole E2E certificate-identity fixture. It creates only the
// explicitly requested CA, leaf subject, and SANs; it owns no product trust
// default, transport admission, certificate validation, or compatibility path.
export function createTlsIdentityFixture(
  storage,
  { caCommonName, serverCommonName, subjectAltNames, validityDays = 1 },
) {
  if (!Array.isArray(subjectAltNames) || subjectAltNames.length === 0) {
    throw new Error('TLS fixture requires at least one explicit subjectAltName');
  }
  const caSubject = requireIdentityPart(caCommonName, 'caCommonName');
  const serverSubject = requireIdentityPart(serverCommonName, 'serverCommonName');
  const sans = subjectAltNames.map(requireSubjectAltName);
  if (!Number.isSafeInteger(validityDays) || validityDays < 1 || validityDays > 30) {
    throw new Error('TLS fixture validityDays must be an integer from 1 to 30');
  }
  fs.mkdirSync(storage, { recursive: true });

  const caKey = path.join(storage, 'worker-test-ca.key');
  const caCertificate = path.join(storage, 'worker-test-ca.pem');
  const serverKey = path.join(storage, 'worker-test-server.key');
  const serverCsr = path.join(storage, 'worker-test-server.csr');
  const serverCertificate = path.join(storage, 'worker-test-server.pem');
  const extensions = path.join(storage, 'worker-test-server.ext');
  fs.writeFileSync(extensions, [
    'basicConstraints=critical,CA:FALSE',
    'keyUsage=critical,digitalSignature,keyEncipherment',
    'extendedKeyUsage=serverAuth',
    `subjectAltName=${sans.join(',')}`,
  ].join('\n'));
  runOpenSsl([
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', String(validityDays),
    '-subj', `/CN=${caSubject}`, '-keyout', caKey, '-out', caCertificate,
  ], 'create Worker E2E CA');
  runOpenSsl([
    'req', '-newkey', 'rsa:2048', '-nodes', '-sha256',
    '-subj', `/CN=${serverSubject}`, '-keyout', serverKey, '-out', serverCsr,
  ], 'create Worker E2E server CSR');
  runOpenSsl([
    'x509', '-req', '-sha256', '-days', String(validityDays), '-in', serverCsr,
    '-CA', caCertificate, '-CAkey', caKey, '-CAcreateserial',
    '-extfile', extensions, '-out', serverCertificate,
  ], 'sign Worker E2E server certificate');
  return { caCertificate, serverCertificate, serverKey };
}

// The persistent local-k3d consumer validates the exact existing fixture
// before reusing it. Return only a Boolean: invalid or malformed identity is
// a rotation request, never a trust fallback or an exposed private-key error.
export function isTlsIdentityUsable(
  { caCertificate, serverCertificate, serverKey, host },
  { now = Date.now(), minRemainingMs = 0 } = {},
) {
  try {
    if (typeof host !== 'string' || !host || !Number.isFinite(now)
      || !Number.isFinite(minRemainingMs) || minRemainingMs < 0) return false;
    const ca = new X509Certificate(caCertificate);
    const server = new X509Certificate(serverCertificate);
    const validNow = (certificate) => Date.parse(certificate.validFrom) <= now
      && Date.parse(certificate.validTo) > now + minRemainingMs;
    return validNow(ca) && validNow(server) && ca.ca && !server.ca
      && ca.verify(ca.publicKey) && ca.subject === server.issuer
      && server.verify(ca.publicKey)
      && server.checkHost(host) === host
      && server.checkPrivateKey(createPrivateKey(serverKey));
  } catch {
    return false;
  }
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === 'verify') {
    const [caPath, certificatePath, keyPath, host, remainingSeconds] = process.argv.slice(3);
    if (!caPath || !certificatePath || !keyPath || !host || !/^\d+$/u.test(remainingSeconds ?? '')) {
      throw new Error('usage: tls_identity_fixture.mjs verify <ca> <certificate> <key> <host> <min-remaining-seconds>');
    }
    const usable = isTlsIdentityUsable({
      caCertificate: fs.readFileSync(caPath),
      serverCertificate: fs.readFileSync(certificatePath),
      serverKey: fs.readFileSync(keyPath), host,
    }, { minRemainingMs: Number(remainingSeconds) * 1_000 });
    if (!usable) process.exitCode = 1;
  } else {
    const [storage, caCommonName, serverCommonName, ...rest] = process.argv.slice(2);
    const daysOption = rest.at(-2) === '--days' ? Number(rest.at(-1)) : undefined;
    const subjectAltNames = daysOption === undefined ? rest : rest.slice(0, -2);
    if (!storage || !caCommonName || !serverCommonName || subjectAltNames.length === 0) {
      throw new Error(
        'usage: tls_identity_fixture.mjs <storage> <ca-common-name> <server-common-name> <SAN>... [--days 1..30]',
      );
    }
    createTlsIdentityFixture(storage, { caCommonName, serverCommonName, subjectAltNames,
      ...(daysOption === undefined ? {} : { validityDays: daysOption }) });
  }
}
