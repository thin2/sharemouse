"""macOS: GUI owning a daemon and GUI attaching to a daemon both quit cleanly.

Uses dummy input and named pasteboards. Never changes login registration.
"""
import json
import os
import socket
import subprocess
import sys
import time
from sharing_session_smoke import BINARY, Cluster


def run(cluster, existing):
    node = cluster.start("existing" if existing else "owned")
    directory = cluster.directory / node["name"]
    if not existing:
        cluster.stop(node)
    args = [str(BINARY), "--config", str(directory / "config.toml"),
            "--cert-path", str(directory / "cert.pem")]
    environment = dict(os.environ, HOME=str(directory), LAN_MOUSE_DISABLE_AUTOSTART="1",
                       LAN_MOUSE_TEST_PASTEBOARD=f"sharemouse-test-resident-{directory.name}")
    with open(directory / "resident-gtk.log", "wb") as log:
        app = subprocess.Popen(args + ["--hidden"], env=environment, stdout=log, stderr=log)
        client = None
        try:
            deadline = time.monotonic() + 10
            while True:
                assert app.poll() is None, "GUI exited before connecting"
                try:
                    client = socket.socket(socket.AF_UNIX)
                    client.settimeout(1)
                    client.connect(str(directory / "Library/Caches/lan-mouse-socket.sock"))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    client.close()
                    assert time.monotonic() < deadline, "GUI did not start daemon"
                    time.sleep(0.02)
            # Wait for GTK to attach to the daemon, including the initial Sync.
            time.sleep(2)
            assert app.poll() is None, "hidden GUI exited during startup"
            client.sendall(json.dumps("PresentWindow").encode() + b"\n")
            result = subprocess.run(args + ["cli", "quit"], env=environment,
                                    capture_output=True, timeout=12)
            assert result.returncode == 0, result.stderr.decode(errors="replace")
            assert app.wait(timeout=10) == 0, "GUI reported failure after daemon quit"
            if existing:
                assert node["process"].wait(timeout=4) == 0
                cluster.stop(node)
            print(f"PASS: {'existing' if existing else 'owned'} daemon and GUI quit gracefully", flush=True)
        finally:
            if client is not None:
                client.close()
            if app.poll() is None:
                app.terminate()
                app.wait(timeout=4)
        content = (directory / "resident-gtk.log").read_text(errors="replace")
        assert "panicked at" not in content and "CRITICAL" not in content, content


def main():
    if sys.platform != "darwin":
        raise SystemExit("Uses macOS HOME-based isolated IPC sockets.")
    cluster = Cluster()
    try:
        run(cluster, existing=True)
        run(cluster, existing=False)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()


if __name__ == "__main__":
    main()
