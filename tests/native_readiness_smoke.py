"""Check unavailable macOS backends without configuring any input barriers.

Uses silent permission checks. Never grants/revokes permissions or injects input.
Failure-path assertions run only when this executable lacks native permission.
"""
from sharing_session_smoke import Cluster


def run(cluster):
    native = cluster.start("native", native=True)
    dummy = cluster.start("dummy")
    cluster.wait(lambda: all(key in native["events"] for key in ("CaptureStatus", "EmulationStatus")),
                 "native status")
    capture_disabled = native["events"]["CaptureStatus"] == "Disabled"
    emulation_disabled = native["events"]["EmulationStatus"] == "Disabled"
    unavailable = capture_disabled or emulation_disabled
    if unavailable:
        assert native["errors"], "unavailable native backend did not report its reason"
    for node, peer in ((native, dummy), (dummy, native)):
        cluster.send(node, {"AuthorizeKey": [peer["name"], peer["events"]["PublicKeyFingerprint"]]})
    cluster.wait(lambda: all(n["events"].get("AuthorizedUpdated") for n in cluster.nodes),
                 "authorized pair")
    cluster.connect_all()
    cluster.wait(lambda: cluster.all_phase("Stopped"), "unavailable backends still permit stop")
    if not unavailable:
        for primary in (native, dummy):
            cluster.send(primary, "StartSharing")
            cluster.wait(lambda: cluster.all_phase("Running", primary["events"]["PublicKeyFingerprint"]),
                         "native backend confirms readiness without input barriers")
            cluster.send(primary, "StopSharing")
            cluster.wait(lambda: cluster.all_phase("Stopped"), "native release confirmation")
        print("PASS: native Mac initialization/readiness/release in both roles without injecting input")
        print("SKIP: permission-denied recovery; native permissions are granted for this executable")
        return
    # No configured clients: even a mistakenly opened gate cannot inject input.
    primaries = (native, dummy) if capture_disabled else (dummy,)
    for primary in primaries:
        for attempt in range(2):
            count = len(native["errors"])
            cluster.send(primary, "StartSharing")
            cluster.wait(lambda: len(native["errors"]) > count
                         and cluster.all_phase("Stopped"),
                         "native readiness failure stops group", seconds=40)
            assert native["events"]["CaptureStatus"] == ("Disabled" if capture_disabled else "Enabled")
            assert native["events"]["EmulationStatus"] == ("Disabled" if emulation_disabled else "Enabled")
        print(f"PASS: {primary['name']} primary blocked by unavailable backend; next start retries",
              flush=True)
    log = (cluster.directory / "native/daemon.log").read_text(errors="replace")
    assert "using emulation backend: dummy" not in log, "silent dummy fallback"


if __name__ == "__main__":
    cluster = Cluster()
    try:
        run(cluster)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()
        print(f"Logs: {cluster.directory}", flush=True)
