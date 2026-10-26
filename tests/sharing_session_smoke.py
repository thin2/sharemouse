"""macOS: exercise three real TLS daemons with dummy native backends.

Run after cargo build --workspace: python3 tests/sharing_session_smoke.py
All certificates, sockets, configurations and logs stay inside ignored .local/.
"""

import json
import os
from pathlib import Path
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("LAN_MOUSE_TEST_BINARY", str(ROOT / "target/debug/lan-mouse")))


class Cluster:
    def __init__(self):
        (ROOT / ".local").mkdir(exist_ok=True)
        self.directory = Path(tempfile.mkdtemp(prefix="s-", dir=ROOT / ".local"))
        self.selector = selectors.DefaultSelector()
        self.nodes = []

    def pump(self, timeout=0.05):
        for key, _ in self.selector.select(timeout):
            node = key.data
            data = node["socket"].recv(65536)
            if not data:
                raise AssertionError(f'{node["name"]}: unexpected IPC disconnect')
            node["buffer"] += data
            while b"\n" in node["buffer"]:
                line, node["buffer"] = node["buffer"].split(b"\n", 1)
                event = json.loads(line)
                if isinstance(event, dict):
                    node["events"].update(event)
                    if "Error" in event:
                        node["errors"].append(event["Error"])
                elif isinstance(event, str):
                    node["events"][event] = node["events"].get(event, 0) + 1
                    if event == "HistoryError":
                        node["errors"].append(event)

    def wait(self, predicate, description, seconds=12):
        deadline = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= deadline:
                states = {n["name"]: n["events"].get("SharingState") for n in self.nodes}
                errors = {n["name"]: n["errors"][-3:] for n in self.nodes}
                raise AssertionError(f"{description}: states={states}, errors={errors}")
            self.pump()

    def send(self, node, request):
        node["socket"].sendall(json.dumps(request).encode() + b"\n")

    def start(self, name, existing=None, native=False):
        directory = self.directory / name
        home = directory
        cache = home / "Library" / "Caches"
        cache.mkdir(parents=True, exist_ok=True)
        config = directory / "config.toml"
        if existing is None:
            with socket.socket() as port_socket:
                port_socket.bind(("127.0.0.1", 0))
                data_port = port_socket.getsockname()[1]
            config.write_text(
                f'port = 0\ndata_port = {data_port}\n'
                + ('' if native else 'capture_backend = "dummy"\nemulation_backend = "dummy"\n')
                + f'receive_dir = "{directory / "received"}"\n'
            )
        log = open(directory / ("restart.log" if existing else "daemon.log"), "wb")
        environment = dict(os.environ, HOME=str(home), LAN_MOUSE_LOG_LEVEL="info",
                           XDG_CONFIG_HOME=str(home / ".config"),
                           LAN_MOUSE_DISABLE_AUTOSTART="1",
                           LAN_MOUSE_TEST_PASTEBOARD=f"sharemouse-test-{self.directory.name}-{name}")
        process = subprocess.Popen(
            [str(BINARY), "--config", str(config),
             "--cert-path", str(directory / "cert.pem"), "daemon"],
            env=environment, stdout=log, stderr=subprocess.STDOUT,
        )
        node = dict(name=name, process=process, log=log, events={}, errors=[], buffer=b"")
        self.nodes.append(node)
        deadline = time.monotonic() + 8
        while True:
            client = socket.socket(socket.AF_UNIX)
            try:
                client.connect(str(cache / "lan-mouse-socket.sock"))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                client.close()
                if process.poll() is not None or time.monotonic() > deadline:
                    raise AssertionError(f"{name}: daemon failed to start ({process.poll()}); see {log.name}")
                time.sleep(0.02)
        client.setblocking(False)
        node["socket"] = client
        self.selector.register(client, selectors.EVENT_READ, node)
        self.wait(lambda: "PublicKeyFingerprint" in node["events"], f"{name}: initial sync")
        return node

    def stop(self, node):
        if "socket" in node:
            self.selector.unregister(node["socket"])
            node.pop("socket").close()
        if node["process"].poll() is None:
            node["process"].send_signal(signal.SIGINT)
            try:
                node["process"].wait(timeout=4)
            except subprocess.TimeoutExpired:
                node["process"].kill()
                node["process"].wait(timeout=4)
        node["log"].close()
        if node in self.nodes:
            self.nodes.remove(node)

    def all_phase(self, phase, primary=None):
        return all(
            n["events"].get("SharingState", {}).get("phase") == phase
            and (primary is None or n["events"]["SharingState"]["primary"] == primary)
            for n in self.nodes
        )

    def connect_all(self):
        for index, node in enumerate(self.nodes):
            for peer in self.nodes[index + 1:]:
                self.send(node, {"ConnectDataSession": {
                    "device": peer["events"]["PublicKeyFingerprint"],
                    "addr": f'127.0.0.1:{peer["events"]["DataPortChanged"][0]}',
                }})

    def run(self):
        a, b, c = [self.start(name) for name in ("a", "b", "c")]
        for node in self.nodes:
            for peer in self.nodes:
                if peer is not node:
                    self.send(node, {"AuthorizeKey": [peer["name"], peer["events"]["PublicKeyFingerprint"]]})
        self.wait(lambda: all(len(n["events"].get("AuthorizedUpdated", {})) == 2 for n in self.nodes),
                  "all authorization updates applied")
        self.connect_all()
        self.wait(lambda: self.all_phase("Stopped"), "initial unanimous stop")
        print("PASS: three TLS daemons confirmed initial stop", flush=True)
        self.send(a, "StartSharing")
        self.wait(lambda: self.all_phase("Running", a["events"]["PublicKeyFingerprint"]), "select primary a")
        self.send(b, "StartSharing")
        self.wait(lambda: bool(b["errors"]), "reject competing primary")
        assert self.all_phase("Running", a["events"]["PublicKeyFingerprint"])
        print("PASS: selected primary a; competing start rejected", flush=True)
        self.send(c, "StopSharing")
        self.wait(lambda: self.all_phase("Stopped"), "stop from follower c")
        self.send(b, "StartSharing")
        self.wait(lambda: self.all_phase("Running", b["events"]["PublicKeyFingerprint"]), "handover to b")
        print("PASS: follower stop acknowledged by all; handover to b", flush=True)
        self.stop(c)
        self.wait(lambda: self.all_phase("Waiting"), "partition closes input sessions")
        self.send(a, "StartSharing")
        self.wait(lambda: bool(a["errors"]), "missing member blocks takeover")
        assert self.all_phase("Waiting")
        c = self.start("c", existing=c)
        self.wait(lambda: self.all_phase("Stopped"), "restart requires fresh stop confirmation")
        self.send(c, "StartSharing")
        self.wait(lambda: self.all_phase("Running", c["events"]["PublicKeyFingerprint"]), "select restarted c")
        self.send(a, "StopSharing")
        self.wait(lambda: self.all_phase("Stopped"), "final stop")
        print("PASS: disconnect blocks takeover; automatic reconnect requires fresh confirmation", flush=True)
        leader = min(self.nodes, key=lambda n: n["events"]["PublicKeyFingerprint"])
        leader_name = leader["name"]
        leader_fingerprint = leader["events"]["PublicKeyFingerprint"]
        self.stop(leader)
        self.wait(lambda: self.all_phase("Waiting"), "coordinator disconnect")
        leader = self.start(leader_name, existing=leader)
        assert leader["events"]["PublicKeyFingerprint"] == leader_fingerprint
        self.connect_all()
        self.wait(lambda: self.all_phase("Stopped"), "restarted coordinator establishes new epoch")
        self.send(leader, "StartSharing")
        self.wait(lambda: self.all_phase("Running", leader_fingerprint), "start after coordinator restart")
        self.send(leader, "StopSharing")
        self.wait(lambda: self.all_phase("Stopped"), "stop after coordinator restart")
        print("PASS: coordinator restart fences old epoch before another start", flush=True)
        if "--gtk" in sys.argv:
            self.gtk_smoke(leader)

    def gtk_smoke(self, node, during=None):
        directory = self.directory / node["name"]
        log_path = directory / "gtk.log"
        with open(log_path, "wb") as log:
            process = subprocess.Popen(
                [str(BINARY), "--config", str(directory / "config.toml"),
                 "--cert-path", str(directory / "cert.pem")],
                env=dict(os.environ, HOME=str(directory), LAN_MOUSE_LOG_LEVEL="info",
                         XDG_CONFIG_HOME=str(directory / ".config"),
                         LAN_MOUSE_DISABLE_AUTOSTART="1"),
                stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    assert process.poll() is None, "GTK exited during startup"
                    self.pump()
                if during is not None:
                    during()
                self.send(node, "StartSharing")
                self.wait(lambda: self.all_phase("Running", node["events"]["PublicKeyFingerprint"]),
                          "GTK observes active session")
                self.send(node, "StopSharing")
                self.wait(lambda: self.all_phase("Stopped"), "GTK observes confirmed stop")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=4)
        content = log_path.read_text(errors="replace")
        assert "CRITICAL" not in content and "panicked at" not in content, f"GTK failure; see {log_path}"
        print("PASS: GTK startup and sharing state updates without panic or CRITICAL", flush=True)


def main():
    if sys.platform != "darwin":
        raise SystemExit("This smoke test uses isolated macOS HOME-based IPC sockets.")
    cluster = Cluster()
    try:
        cluster.run()
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()
    for log in cluster.directory.glob("*/*.log"):
        content = log.read_text(errors="replace")
        assert "panicked at" not in content, f"panic in {log}"
    print("PASS: clean shutdown; no daemon panic")


if __name__ == "__main__":
    main()
