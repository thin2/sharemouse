"""Two isolated daemons: capture -> authenticated DTLS -> emulation in both roles.

Native backends are explicitly dummy. No system input is generated.
"""
import time

from enrollment_session_smoke import EnrollmentCluster, change_port, clients


def run(cluster):
    a, b = [cluster.start(name) for name in ("a", "b")]
    for node in (a, b):
        change_port(cluster, node)
    cluster.send(a, {"HostGroup": {"port": 0}})
    cluster.wait(lambda: a["events"].get("EnrollmentState", {}).get("status") == "hosting",
                 "host enrollment")
    address = "127.0.0.1:" + a["events"]["EnrollmentState"]["addresses"][0].rsplit(":", 1)[1]
    cluster.pair(a, b, address)
    cluster.wait(lambda: cluster.all_phase("Stopped"), "initial stop")
    handles = {}
    for node in (a, b):
        handle, _, _ = clients(cluster, node)[0]
        handles[node["name"]] = handle
        cluster.send(node, {"UpdatePosition": [handle, "left"]})

    for primary, receiver in ((a, b), (b, a)):
        path = cluster.directory / receiver["name"] / "daemon.log"
        baseline = path.stat().st_size
        cluster.send(primary, "StartSharing")
        cluster.wait(lambda: cluster.all_phase("Running", primary["events"]["PublicKeyFingerprint"]),
                     "selected primary is ready")

        def received():
            with open(path, "rb") as log:
                log.seek(baseline)
                return log.read().decode(errors="replace")

        expected = ("motion(", "key(KeyA, 1)", "key(KeyA, 0)",
                    "button(left, 1)", "button(left, 0)")
        cluster.wait(lambda: all(f") {event}" in received() for event in expected),
                     "motion, keyboard and mouse button reach emulation", seconds=20)
        cluster.wait(lambda: primary["events"].get("CurrentTarget") == handles[primary["name"]],
                     "authenticated acknowledgement selects target")
        cluster.send(primary, "StopSharing")
        cluster.wait(lambda: cluster.all_phase("Stopped"), "confirmed stop")
        assert primary["events"].get("CurrentTarget") is None
        # Let already-written log bytes settle, then prove the stop gate holds.
        deadline = time.monotonic() + 0.25
        while time.monotonic() < deadline:
            cluster.pump()
        count = received().count("received event:")
        deadline = time.monotonic() + 0.4
        while time.monotonic() < deadline:
            cluster.pump()
        assert received().count("received event:") == count, "input continued after stop"
        print(f"PASS: {primary['name']} -> {receiver['name']} actual DTLS input, target and stop", flush=True)


if __name__ == "__main__":
    cluster = EnrollmentCluster()
    try:
        run(cluster)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()
        print(f"Logs: {cluster.directory}", flush=True)
