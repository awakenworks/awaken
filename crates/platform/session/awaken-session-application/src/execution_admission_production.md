The shared production application cannot change its selected sources:

```compile_fail,E0599
use std::sync::Arc;
use awaken_session_application::SessionApplication;
use awaken_worker_contract::WorkerIncarnationSource;
fn change(app: &SessionApplication, other: Arc<dyn WorkerIncarnationSource>) {
    app.set_worker_observation_source(other);
}
```

```compile_fail,E0599
use std::sync::Arc;
use awaken_session_application::SessionApplication;
use awaken_dispatch_contract::DispatchOperationalFeed;
fn change(app: &SessionApplication, other: Arc<dyn DispatchOperationalFeed>) {
    app.set_dispatch_observation_source(other);
}
```

```compile_fail,E0599
use std::sync::Arc;
use awaken_session_application::SessionApplication;
use awaken_session_execution_contract::SessionPreparationPlacement;
fn change(app: &SessionApplication, other: Arc<dyn SessionPreparationPlacement>) {
    app.set_session_preparation_placement(other);
}
```
