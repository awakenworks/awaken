"""Preserve Lefthook's selected command and argv at its Windows sh boundary."""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import tempfile

from _native_shell import RUST_WINDOWS_SHELL_ARGUMENT


# Lefthook's Windows executor supplies this exact raw frame, without escaping
# quotes inside the command. Reading CRT argv would already lose those bytes.
# Recover the original UTF-16 suffix, then quote it for the real MSYS shell.
# https://github.com/evilmartians/lefthook/blob/v2.1.10/internal/run/controller/exec/exec_windows.go
SH_SOURCE = RUST_WINDOWS_SHELL_ARGUMENT + r'''use std::{env, ffi::OsString, os::windows::{ffi::OsStringExt, process::CommandExt}, process::{Command, exit}};
#[link(name="kernel32")]
extern "system" { fn GetCommandLineW() -> *const u16; }
fn main() {
    let pointer = unsafe { GetCommandLineW() };
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 { length += 1; }
    let raw = unsafe { std::slice::from_raw_parts(pointer, length) };
    let arguments: Vec<OsString> = if raw.starts_with(&[34, 115, 104, 34]) {
        // The main executor explicitly spells argv0 as "sh" and leaves its
        // command unescaped. Predicate commands use normal Go CRT encoding.
        assert!(raw.starts_with(&[34, 115, 104, 34, 32, 45, 99, 32, 34])
                && raw.last() == Some(&34), "expected exact Lefthook sh -c frame");
        vec![OsString::from("-c"), OsString::from_wide(&raw[9..raw.len() - 1])]
    } else {
        env::args_os().skip(1).collect()
    };
    assert!(!arguments.is_empty(), "expected a native shell command");
    let shell = env::var_os("AWAKEN_NATIVE_SH_COMMAND").expect("selected native shell");
    let mut command = Command::new(shell);
    for argument in arguments {
        command.raw_arg(windows_shell_argument(&argument));
    }
    let status = command.status().expect("native shell launch");
    exit(status.code().unwrap_or(1));
}
'''


def run_pre_commit():
    from ci_performance import task_directory, require_active_task
    from formal_run_evidence import write_record
    directory = task_directory()
    if directory is None:
        raise ValueError('native Lefthook requires its active observation context')
    require_active_task(directory)
    selected = Path(os.environ['LEFTHOOK_CONFIG']).resolve()
    projection = json.loads((directory / 'hook-config.json').read_text(encoding='utf-8'))
    if (selected.parent != directory.resolve() or not selected.name.startswith('hook-config-')
            or hashlib.sha256(selected.read_bytes()).hexdigest() != projection['native_digest']):
        raise ValueError('native Lefthook requires the original owned configuration projection')
    command = ['lefthook', 'run', 'pre-commit', '--no-auto-install']
    if os.name != 'nt':
        os.execvpe(command[0], command, os.environ)
    from _native_shell import bash_executable
    bash = Path(bash_executable())
    candidates = (bash.with_name('sh.exe'), bash.parent.parent / 'usr/bin/sh.exe')
    shell = next((path for path in candidates if path.is_file()), None)
    if shell is None:
        raise RuntimeError('native Lefthook requires the selected Git installation sh.exe')
    environment = dict(os.environ)
    if not environment.get('GIT_INDEX_FILE'):
        # Bind the real default index for a direct `lefthook run pre-commit`.
        # Existing Git/alternate-index selections are retained verbatim.
        environment['GIT_INDEX_FILE'] = subprocess.check_output(
            ['git', 'rev-parse', '--path-format=absolute', '--git-path', 'index'],
            env=environment).decode('utf-8').strip()
    compiler = [environment.get('RUSTC') or 'rustc']
    if environment.get('RUSTC_WRAPPER'):
        compiler.insert(0, environment['RUSTC_WRAPPER'])
    with tempfile.TemporaryDirectory(prefix='lefthook-native-shell-', dir=directory) as raw:
        owned = Path(raw)
        source, executable = owned / 'sh.rs', owned / 'sh.exe'
        source.write_text(SH_SOURCE, encoding='utf-8', newline='\n')
        with (directory / 'native-lefthook-compile.log').open('xb') as log:
            compiled = subprocess.run([*compiler, '--edition=2021', '--crate-name',
                                       'awaken_native_sh', str(source), '-o', str(executable)],
                                      stdout=log, stderr=subprocess.STDOUT, env=environment, timeout=120)
        if compiled.returncode:
            raise RuntimeError('native Lefthook shell compilation failed; see native-lefthook-compile.log')
        write_record(directory / 'native-lefthook.json', {
            'kind': 'native-lefthook-shell', 'source_sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
            'executable_sha256': hashlib.sha256(executable.read_bytes()).hexdigest(),
            'selected_shell': str(shell), 'configuration_source_digest': projection['source_digest'],
            'configuration_native_digest': projection['native_digest'],
            'scope': 'original Lefthook inventory, filtering, argv and staging; raw Windows shell transport only',
        })
        environment.update(PATH=str(owned) + os.pathsep + environment['PATH'],
                           AWAKEN_NATIVE_SH_COMMAND=str(shell))
        return subprocess.call(command, env=environment)
