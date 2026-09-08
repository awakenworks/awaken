// Test-peer reader of Runtime's resource prompt, never a mount/path resolver.
// A resource mission has exactly one File. Native and ACP consume the same data.
export function singleFileReference(text) {
  const records = [...text.matchAll(/Read-only file reference \(JSON data\): ([^\r\n]+)/gu)];
  if (records.length !== 1) throw new Error('Resource mission requires exactly one Runtime File reference');
  const reference = JSON.parse(records[0][1]);
  if (!reference || ['file_id', 'binding_id', 'path'].some(
    field => typeof reference[field] !== 'string' || !reference[field].trim(),
  )) throw new Error('Runtime File reference requires identity and path');
  return reference;
}
