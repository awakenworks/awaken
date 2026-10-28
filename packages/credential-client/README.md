# Native credential client

Generic, execution-only recipient envelope consumers for Node and Python.
Login selectors and browser/Android logic belong to the application. Never
install this SDK, its native bearer token or recipient key in the model's
filesystem, frontend bundle or unrestricted process environment.

Install the local packages while developing this workspace:

```powershell
npm install --ignore-scripts <path-to-native>/packages/credential-client
python -m pip install -e <path-to-native>/packages/credential-client/python
```

Node private executor:

```javascript
import {CredentialClient} from '@awaken/credential-client';
const client = new CredentialClient({
  url: nativeUrl, token: privateServiceToken, consumer: consumerId,
  keyFile: privateRecipientFile,
  materialType: 'app.login/v1', handlingMode: 'application_managed',
});
// Private UI ingress only; values never enter model messages or tool arguments.
const credential = await client.enter(accountRef, privateFields);
// runtimeRunId comes from authenticated runtime transport, not model arguments.
const execution = await client.resolveExecution(accountRef, {run_id: runtimeRunId});
// Optional agent_id is an assertion checked against Native, never a grant.
const binding = {account_ref: accountRef, credential, execution,
  operation_id: operationId, epoch: executorEpoch, plan_hash: planHash, target};
await client.withMaterial(binding, async fields => {
  await fields.useField('password', value => restrictedExecutor.fillPassword(value));
});
```

Python uses `from awaken_credentials import CredentialClient` with
`CredentialClient(url, token_file, recipient_file, consumer, material_type,
handling_mode)`, then `resolve_execution`, `current` and
`consume(binding, callback)`. The callback receives `use_field(name, writer)`.
It returns no material through the SDK response.

Both clients require the registered mode, exact target/holder/revision and the
full authenticated execution context. Native revalidates current authority on
every check. Keys, registry, Vault and application user authorization remain
deployment responsibilities. See [ADR-0100](../../docs/adr/0100-credential-handling-and-execution-binding.md)
for the full contract, binding/recovery rules and qualification limits.
Native and external ACP Agents share this authority. Execution correlation is
sent only to HTTP MCP endpoints explicitly declaring
`?awaken_execution_context=run-agent-v1`; see
[ADR-0101](../../docs/adr/0101-acp-application-execution-context.md).
