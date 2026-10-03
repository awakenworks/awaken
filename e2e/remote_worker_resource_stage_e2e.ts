// The stage gate owns the Resource partition of the canonical remote Worker
// scenario. Brain/Hand lazy provisioning remains in the deterministic suite.
import { runRemoteWorkerScenario } from './hand_brain_remote_worker_e2e.ts';

runRemoteWorkerScenario({ resourceOnly: true }).catch((error) => {
  console.error('REMOTE WORKER RESOURCE E2E FAIL:', error);
  process.exitCode = 1;
});
