#!/usr/bin/env python3
"""Real Orca -> AT-SPI -> Speech Dispatcher acceptance over existing Playwright.

No reader implementation is mocked. Xvfb/private D-Bus/settings isolate the
operator desktop. ALSA writes synthesized audio to a test file instead of speakers.
R1 dependencies+reader+speech ready -> run browser assertions; R2 missing/crashed
component -> fail before acceptance; R3 any outcome -> terminate owned children;
R4 reader output without real synthesized PCM -> fail, never call it audio proof.
"""
import json
import os
import pty
import re
import select
import threading
from pathlib import Path
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import time
import tempfile
import wave

REPO = Path(__file__).resolve().parents[2]


def await_condition(check, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.1)
    raise RuntimeError(f"Timed out: {description}")


def unused_port():
    with socket.socket() as handle:
        handle.bind(("127.0.0.1", 0))
        return str(handle.getsockname()[1])


def private_session(root):
    for folder in ["runtime", "config", "data", "orca", "speech", "speech-log"]:
        (root / folder).mkdir(mode=0o700)
    os.environ.update(
        XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"),
        XDG_DATA_HOME=str(root / "data"), GSETTINGS_BACKEND="keyfile",
        NO_AT_BRIDGE="0", ACCESSIBILITY_ENABLED="1", GTK_A11Y="always",
        GDK_BACKEND="x11", GIO_USE_VFS="local", XDG_SESSION_TYPE="x11",
        AWAKEN_ORCA_LOG=str(root / "orca.log"), AWAKEN_UIUX_BROWSERS="chromium",
        AWAKEN_E2E_BACKEND_PORT=unused_port(), AWAKEN_E2E_WEB_PORT=unused_port(),
    )
    os.environ.pop("WAYLAND_DISPLAY", None)
    os.environ.pop("AT_SPI_BUS_ADDRESS", None)
    if os.environ.get("AWAKEN_UIUX_LIVE") == "1":
        raise RuntimeError("Reader debug logs require synthetic credentials; disable live mode")
    (root / "alsa.conf").write_text(
        f'pcm.!default {{ type file slave.pcm "null" file "{root / "speech.pcm"}" format "raw" }}\n'
        'pcm.null { type null }\n')
    os.environ["ALSA_CONFIG_PATH"] = str(root / "alsa.conf")
    os.environ["SPEECHD_ADDRESS"] = "unix_socket:" + str(root / "runtime/speech.sock")
    (root / "speech/speechd.conf").write_text(
        'LogLevel 5\nAudioOutputMethod "alsa"\nAudioALSADevice "default"\n'
        'AddModule "espeak-ng" "sd_espeak-ng" "/etc/speech-dispatcher/modules/espeak-ng.conf"\n'
        'DefaultModule espeak-ng\nDefaultLanguage en\n')
    (root / "orca/user-settings.conf").write_text(json.dumps({
        "general": {"activeProfile": ["Default", "default"],
                    "startingProfile": ["Default", "default"], "enableSpeech": True,
                    "speechServerFactory": "speechdispatcherfactory",
                    "speechServerInfo": ["espeak-ng", "espeak-ng"], "enableBraille": False},
        "pronunciations": {}, "keybindings": {},
        "profiles": {"default": {"profile": ["Default", "default"],
                                  "pronunciations": {}, "keybindings": {}}},
    }, indent=2))
    children, handles = [], []
    # R5 Orca's ordinary debug file is block-buffered. A real terminal makes
    # Python line-buffer the official output; capture it without changing Orca.
    master, slave = pty.openpty()
    stop_capture = threading.Event()

    def capture_reader():
        with (root / "orca.log").open("wb", buffering=0) as log:
            while not stop_capture.is_set():
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    log.write(chunk)

    collector = threading.Thread(target=capture_reader, daemon=True)
    collector.start()

    def launch(args, name, stdout=None):
        log = (root / name).open("w")
        handles.append(log)
        child = subprocess.Popen(args, cwd=REPO, stdout=stdout or log,
                                 stderr=log, start_new_session=True)
        children.append(child)
        return child

    result = None
    try:
        display = launch(["Xvfb", "-displayfd", "1", "-screen", "0", "1280x900x24",
                          "-nolisten", "tcp"], "xvfb.log", subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(display.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=10):
                raise RuntimeError("Xvfb did not provide its display number")
            number = display.stdout.readline().decode().strip()
        if not number.isdigit():
            raise RuntimeError("Xvfb startup failed")
        os.environ["DISPLAY"] = ":" + number
        subprocess.run(["gsettings", "set", "org.gnome.desktop.a11y.applications",
                        "screen-reader-enabled", "true"], check=True)
        speech = launch(["speech-dispatcher", "-s", "-t", "0", "-C", str(root / "speech"),
                         "-S", str(root / "runtime/speech.sock"),
                         "-P", str(root / "runtime/speech.pid"),
                         "-L", str(root / "speech-log")], "speech.log")
        await_condition(lambda: speech.poll() is None and (root / "runtime/speech.sock").exists(),
                        "Speech Dispatcher socket")
        reader = launch(["orca", "--user-prefs", str(root / "orca"), "--debug-file",
                         os.ttyname(slave), "--enable", "speech"], "orca-stdout.log")
        await_condition(lambda: reader.poll() is None and (root / "orca.log").exists()
                        and "SPEECH: Using speech server factory: speechdispatcherfactory"
                        in (root / "orca.log").read_text(), "real Orca speech connection")
        browser = launch(["pnpm", "--dir", "web", "exec", "playwright", "test",
                          "mcp-config.spec.ts", "--grep", "Orca acceptance", "--headed",
                          "--output", str(root / "browser-artifacts")], "browser.log")
        result = browser.wait(timeout=360)
        if reader.poll() is not None or speech.poll() is not None:
            raise RuntimeError("Reader or synthesizer exited during acceptance")
    finally:
        for child in reversed(children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        stop_capture.set()
        collector.join(timeout=5)
        os.close(master)
        os.close(slave)
        for handle in handles:
            handle.close()
    if result != 0:
        raise RuntimeError(f"Browser acceptance failed ({result}); see {root / 'browser.log'}")
    # R6 ALSA captures the real PCM stream. Its parameters are checked against
    # Speech Dispatcher's module log; WAV wrapping does not resynthesize audio.
    pcm = (root / "speech.pcm").read_bytes()
    dispatch_log = (root / "speech-log/speech-dispatcher.log").read_text(errors="replace")
    expected_format = {"bits": {"16"}, "num_channels": {"1"}, "sample_rate": {"22050"}}
    for parameter, expected in expected_format.items():
        observed = set(re.findall(r"Got audio parameter " + parameter + r" (\d+)", dispatch_log))
        if observed != expected:
            raise RuntimeError(f"Unexpected recorded audio {parameter}: {observed}")
    if len(pcm) < 1000 or len(pcm) % 2 or not any(pcm):
        raise RuntimeError("No valid synthesized PCM evidence")
    audio = {"frames": len(pcm) // 2, "sample_rate": 22050, "channels": 1}
    with wave.open(str(root / "speech.wav"), "wb") as recording:
        recording.setparams((1, 2, 22050, 0, "NONE", "not compressed"))
        recording.writeframes(pcm)
    with wave.open(str(root / "speech.wav"), "rb") as recording:
        if recording.readframes(recording.getnframes()) != pcm:
            raise RuntimeError("WAV wrapper changed the recorded PCM")
    shutil.copy2(REPO / "web/test-results/results.json", root / "results.json")
    (root / "reader-evidence.json").write_text(json.dumps({
        "pass": True, "reader": subprocess.check_output(["orca", "--version"], text=True).strip(),
        "transport": "AT-SPI", "synthesizer": "Speech Dispatcher / espeak-ng", "audio": audio,
        "audio_sink": "ALSA file; not human listening", "human_participants": 0,
        "physical_mobile_devices_tested": 0,
    }, indent=2) + "\n")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--private-session":
        private_session(Path(sys.argv[2]))
        return
    for executable in ["Xvfb", "dbus-run-session", "orca", "speech-dispatcher", "pnpm", "gsettings"]:
        if not shutil.which(executable):
            raise RuntimeError(f"Reader acceptance dependency missing: {executable}")
    root = Path(os.environ.get("AWAKEN_ORCA_ARTIFACT_DIR", Path(tempfile.gettempdir()) / f"awaken-orca-{time.time_ns()}")).resolve()
    root.mkdir(parents=True, mode=0o700, exist_ok=False)
    # Unix socket paths are bounded; refuse an unusable long artifact path.
    if len(str(root / "runtime/speech.sock").encode()) >= 104:
        raise RuntimeError("Choose a shorter AWAKEN_ORCA_ARTIFACT_DIR (Unix socket path limit)")
    before = subprocess.check_output(["gsettings", "get", "org.gnome.desktop.a11y.applications", "screen-reader-enabled"])
    try:
        command = [str(REPO / "scripts/ci/_provider_environment.sh"), "--exec", "dbus-run-session", "--",
                   "/usr/bin/python3", str(Path(__file__).resolve()), "--private-session", str(root)]
        with (root / "desktop-session.log").open("w") as log:
            subprocess.run(command, cwd=REPO, stdout=log, stderr=log, check=True, timeout=420)
    finally:
        after = subprocess.check_output(["gsettings", "get", "org.gnome.desktop.a11y.applications", "screen-reader-enabled"])
        if before != after:
            raise RuntimeError("Operator screen-reader setting changed unexpectedly")
    print(f"PASS: real Orca acceptance; evidence at {root}")


if __name__ == "__main__":
    main()
