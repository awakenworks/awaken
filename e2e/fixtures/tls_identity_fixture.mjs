import { spawnSync } from 'node:child_process';
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
  { caCommonName, serverCommonName, subjectAltNames },
) {
  if (!Array.isArray(subjectAltNames) || subjectAltNames.length === 0) {
    throw new Error('TLS fixture requires at least one explicit subjectAltName');
  }
  const caSubject = requireIdentityPart(caCommonName, 'caCommonName');
  const serverSubject = requireIdentityPart(serverCommonName, 'serverCommonName');
  const sans = subjectAltNames.map(requireSubjectAltName);
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
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-subj', `/CN=${caSubject}`, '-keyout', caKey, '-out', caCertificate,
  ], 'create Worker E2E CA');
  runOpenSsl([
    'req', '-newkey', 'rsa:2048', '-nodes', '-sha256',
    '-subj', `/CN=${serverSubject}`, '-keyout', serverKey, '-out', serverCsr,
  ], 'create Worker E2E server CSR');
  runOpenSsl([
    'x509', '-req', '-sha256', '-days', '1', '-in', serverCsr,
    '-CA', caCertificate, '-CAkey', caKey, '-CAcreateserial',
    '-extfile', extensions, '-out', serverCertificate,
  ], 'sign Worker E2E server certificate');
  return { caCertificate, serverCertificate, serverKey };
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  const [storage, caCommonName, serverCommonName, ...subjectAltNames] = process.argv.slice(2);
  if (!storage || !caCommonName || !serverCommonName || subjectAltNames.length === 0) {
    throw new Error(
      'usage: tls_identity_fixture.mjs <storage> <ca-common-name> <server-common-name> <SAN>...',
    );
  }
  createTlsIdentityFixture(storage, { caCommonName, serverCommonName, subjectAltNames });
}
