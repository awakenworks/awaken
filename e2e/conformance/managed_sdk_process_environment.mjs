import path from 'node:path';

export function managedSdkOwnerProcessEnvironment(environment, e2eRoot) {
  const configured = environment.AWAKEN_MANAGED_SDK_CARGO_TARGET_DIR;
  const cargoTargetDirectory = path.resolve(
    configured || environment.CARGO_TARGET_DIR || path.join(e2eRoot, '../target'),
  );
  return {
    ...environment,
    CARGO_TARGET_DIR: cargoTargetDirectory,
  };
}
