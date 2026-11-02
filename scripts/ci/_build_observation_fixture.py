"""The observer port for synthetic command fixtures, never a runtime fallback."""
from pathlib import Path
import tempfile
from unittest import mock


def synthetic_build_observations(case):
    # Synthetic subprocess tables do not represent real builds. Their original
    # source/status/evidence assertions stay active; I1-I19 and real Cargo own
    # durable observation coverage. Keep this one fixture port across suites.
    temporary = tempfile.TemporaryDirectory(prefix='synthetic-build-observation-')
    case.addCleanup(temporary.cleanup)
    replacement = mock.patch('ci_performance.TaskRun')
    task = replacement.start()
    case.addCleanup(replacement.stop)
    task.return_value.environment = {
        'AWAKEN_BUILD_RUN_ID': 'synthetic-fixture',
        'AWAKEN_BUILD_RUN_DIRECTORY': str(Path(temporary.name).resolve()),
    }
    return task
