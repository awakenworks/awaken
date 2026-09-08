import { cargoExecutable, cargoScenarioHostBundle, scenarioHandCompanionPath } from '../cargo_binary.mjs';
import { snapshotExecutable } from '../executable_snapshot.mjs';

// Both credential and Resource drivers run this same example. Cargo's example
// directory is not the binary directory: use the existing scenario bundle's
// qualified companion and release-layout helper before exposing the executable.
// R1 ordinary/Docker example + complete companion => the Worker may start;
// R2 absent prebuilt companion => fail here, before waiting for model output.
// Existing Cargo bundle tests own missing-companion and byte-copy boundaries;
// the actual Worker E2Es own startup and credential/resource behavior.
export function credentialReferenceWorkerFixture({ root, features = [] }) {
  const executable = cargoExecutable({ cwd: root, packageName: 'awaken-cli',
    targetName: 'credential_reference_worker', targetKind: 'example', features });
  const { handCompanion } = cargoScenarioHostBundle({ cwd: root });
  snapshotExecutable(handCompanion, scenarioHandCompanionPath(executable));
  return executable;
}
