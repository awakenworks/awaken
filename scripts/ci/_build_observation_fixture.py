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
        'AWAKEN_BUILD_OBSERVATION_KIND': 'synthetic',
    }
    return task


def real_build_observations(case, label="fixture"):
    """Private real observation lifecycle shared by reporter/worker tests."""
    import os
    import subprocess
    from ci_performance import TaskRun
    from formal_sources import detached_git_environment
    temporary = tempfile.TemporaryDirectory()
    case.addCleanup(temporary.cleanup)
    root = Path(temporary.name) / 'repository'
    root.mkdir()
    environment = {key: value for key, value in detached_git_environment().items() if not key.startswith('AWAKEN_BUILD_')}
    environment.update(AWAKEN_BUILD_OBSERVATIONS=str(Path(temporary.name) / 'records'),
                       GIT_AUTHOR_NAME='Build tests', GIT_AUTHOR_EMAIL='build@example.test',
                       GIT_COMMITTER_NAME='Build tests', GIT_COMMITTER_EMAIL='build@example.test')
    subprocess.run(['git', 'init', '-q', str(root)], check=True, env=environment)
    (root / 'source').write_text('original\n')
    (root / '.gitignore').write_text('target/\narchive/\n')
    subprocess.run(['git', '-C', str(root), 'add', 'source', '.gitignore'], check=True, env=environment)
    subprocess.run(['git', '-C', str(root), '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'], check=True, env=environment)
    return TaskRun(label, root, environment), root, environment
