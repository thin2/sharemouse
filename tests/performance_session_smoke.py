"""Record local TLS file throughput and group control round trips.

This measures loopback with dummy input, not physical input latency or LAN speed.
Output: .local/performance-baseline.json. No clipboard contents are collected.
"""
import hashlib
import json
import platform
import statistics
import time
from sharing_session_smoke import BINARY, Cluster, ROOT
from connection_fixture import configure


def run(cluster):
    print(f"Fixtures: {cluster.directory}", flush=True)
    a, b, c, handle = configure(cluster)
    # configure deliberately restarts a daemon to make dummy capture discover
    # the peer identity. A retry that races that restart can fail as expected.
    for node in cluster.nodes:
        node["events"].pop("Messages", None)
        cluster.send(node, "Sync")
    cluster.wait(lambda: all("Messages" in n["events"]
                            and len(n["events"].get("DataSessions", [])) == 2
                            for n in cluster.nodes) and cluster.all_phase("Stopped"),
                 "restarted fixture fully reconnected")
    setup_errors = {n["name"]: n["errors"][:] for n in cluster.nodes if n["errors"]}
    assert all("Connection refused" in error for errors in setup_errors.values() for error in errors), setup_errors
    for node in cluster.nodes:
        node["errors"].clear()
    source = cluster.directory / "throughput.bin"
    # 128 MiB gives control messages a chance to overlap the file stream.
    with source.open("wb") as file:
        chunk = bytes(range(256)) * 4096
        for _ in range(128):
            file.write(chunk)
    start_times = []
    stop_times = []
    began = time.monotonic()
    cluster.send(a, {"SendFiles": {"handle": handle, "paths": [str(source)]}})
    cluster.wait(lambda: a["events"].get("TransferProgress", {}).get("sent", 0) > 0
                 or "TransferFailed" in a["events"], "transfer begins", seconds=60)
    assert "TransferFailed" not in a["events"], a["events"].get("TransferFailed")
    overlapped = 0
    for _ in range(10):
        if "TransferComplete" not in b["events"]:
            overlapped += 1
        t = time.monotonic()
        cluster.send(c, "StartSharing")
        cluster.wait(lambda: cluster.all_phase("Running", c["events"]["PublicKeyFingerprint"]), "start under transfer")
        start_times.append((time.monotonic() - t) * 1000)
        t = time.monotonic()
        cluster.send(c, "StopSharing")
        cluster.wait(lambda: cluster.all_phase("Stopped"), "stop under transfer")
        stop_times.append((time.monotonic() - t) * 1000)
    cluster.wait(lambda: "TransferComplete" in b["events"], "transfer complete", seconds=60)
    elapsed = time.monotonic() - began
    received = cluster.directory / "b/received/throughput.bin"
    assert received.read_bytes() == source.read_bytes()
    errors = {n["name"]: n["errors"] for n in cluster.nodes if n["errors"]}
    result = dict(
        environment=dict(os=platform.platform(), architecture=platform.machine(),
                         transport="loopback TLS", input="dummy", binary=BINARY.name,
                         profile=BINARY.parent.name),
        file_bytes=source.stat().st_size, observation_seconds=elapsed,
        observed_mib_per_second=source.stat().st_size / elapsed / 1048576,
        sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
        start_ms=dict(median=statistics.median(start_times), maximum=max(start_times)),
        stop_ms=dict(median=statistics.median(stop_times), maximum=max(stop_times)),
        rounds_started_before_transfer_completion=overlapped,
        note="Observation includes control rounds; not a pure throughput or native input benchmark.",
        errors=errors,
        setup_connection_refusals=setup_errors,
    )
    (ROOT / ".local/performance-baseline.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2), flush=True)
    assert not errors, errors


if __name__ == "__main__":
    cluster = Cluster()
    try:
        run(cluster)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()
