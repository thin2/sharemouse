"""Isolated production TLS/IPC regressions for receiver decisions and GTK rows.

macOS only; uses the existing dummy-input/named-pasteboard cluster fixtures.
"""
import sys
import time
from pathlib import Path

from sharing_session_smoke import Cluster
from connection_fixture import configure


def run(cluster):
    a, b, _, handle = configure(cluster)
    source = cluster.directory / "source"
    source.mkdir()
    folder = source / "photos"
    folder.mkdir()
    (folder / "new.txt").write_bytes(b"new transfer payload")
    destination = cluster.directory / "b/received"
    (destination / "photos").mkdir(parents=True, exist_ok=True)
    (destination / "photos/local.txt").write_bytes(b"keep me")

    def begin(path):
        for key in ("TransferSent", "TransferFailed", "TransferProgress"):
            a["events"].pop(key, None)
        for key in ("TransferConflicts", "TransferComplete"):
            b["events"].pop(key, None)
        cluster.send(a, {"SendFiles": {"handle": handle, "paths": [str(path)]}})

    def decision(policy):
        cluster.wait(lambda: "TransferConflicts" in b["events"], "recipient conflict decision")
        conflict = b["events"]["TransferConflicts"]
        assert "photos" in conflict["paths"]
        # Holding the dialog open must not consume the file body.
        deadline = time.monotonic() + 0.25
        while time.monotonic() < deadline:
            cluster.pump()
        assert a["events"].get("TransferProgress", {}).get("sent", 0) == 0
        assert "TransferSent" not in a["events"]
        cluster.send(b, {"ResolveTransferConflict": {
            "transfer_id": conflict["transfer_id"], "policy": policy,
        }})

    def success():
        cluster.wait(lambda: "TransferSent" in a["events"] or "TransferFailed" in a["events"],
                     "sender receives committed result")
        assert "TransferFailed" not in a["events"], a["events"].get("TransferFailed")
        cluster.wait(lambda: "TransferComplete" in b["events"], "recipient committed")

    begin(folder)
    decision("overwrite")
    success()
    assert (destination / "photos/new.txt").read_bytes() == b"new transfer payload"
    assert (destination / "photos/local.txt").read_bytes() == b"keep me"

    (folder / "only-after-skip.txt").write_bytes(b"must skip whole folder")
    begin(folder)
    decision("skip")
    success()
    assert not (destination / "photos/only-after-skip.txt").exists()

    begin(folder)
    decision("rename")
    success()
    assert (destination / "photos (1)/only-after-skip.txt").is_file()

    begin(folder)
    decision("cancel")
    cluster.wait(lambda: "TransferFailed" in a["events"], "recipient cancellation reaches sender")
    assert "TransferSent" not in a["events"]
    print("PASS: folder decisions, body gating, committed acknowledgement and cancellation", flush=True)

    # A pending conflict must disappear when its sender disconnects.
    begin(folder)
    cluster.wait(lambda: "TransferConflicts" in b["events"], "pending before disconnect")
    old_key = b["events"]["TransferConflicts"]["transfer_id"]
    cluster.send(a, {"Activate": [handle, True]})
    cluster.send(a, "SaveConfiguration")
    cluster.wait(lambda: "activate_on_startup = true" in
                 (cluster.directory / "a/config.toml").read_text(), "prepare identity rediscovery")
    cluster.stop(a)
    cluster.wait(lambda: "DataSessionDisconnected" in b["events"], "sender disconnect")
    a = cluster.start("a", existing=a)
    cluster.connect_all()
    cluster.wait(lambda: cluster.all_phase("Stopped"), "reconnected group")
    deadline = time.monotonic() + 12
    while True:
        cluster.send(a, {"Enumerate": []})
        cluster.pump()
        if any(h == handle and state.get("peer_fingerprint") == b["events"]["PublicKeyFingerprint"]
               for h, _, state in a["events"].get("Enumerate", [])):
            break
        assert time.monotonic() < deadline, "restarted sender did not relearn DTLS identity"
    cluster.send(a, {"Activate": [handle, False]})
    cluster.send(a, "SaveConfiguration")
    begin(folder)
    cluster.wait(lambda: "TransferConflicts" in b["events"], "retry after reconnect")
    assert b["events"]["TransferConflicts"]["transfer_id"] != old_key
    # An old GUI response cannot act on the replacement transfer.
    cluster.send(b, {"ResolveTransferConflict": {"transfer_id": old_key, "policy": "overwrite"}})
    decision("cancel")
    cluster.wait(lambda: "TransferFailed" in a["events"], "new conflict can still cancel")
    print("PASS: disconnect cleanup and stale conflict response fencing", flush=True)

    if "--gtk" in sys.argv:
        def gui_sends():
            for number in range(2):
                path = source / f"gui-{number}.txt"
                path.write_bytes(f"GUI fixture {number}".encode())
                a["events"].pop("TransferSent", None)
                a["events"].pop("TransferFailed", None)
                b["events"].pop("TransferComplete", None)
                # This runs the real GTK selection -> send -> progress path.
                cluster.send(a, {"SelectTarget": {"paths": [str(path)]}})
                success()
                assert (destination / path.name).read_bytes() == path.read_bytes()
        cluster.gtk_smoke(a, gui_sends)

        def gui_receives():
            for number in range(2):
                path = source / f"receive-gui-{number}.txt"
                path.write_bytes(b"receiver row update")
                begin(path)
                success()
        cluster.gtk_smoke(b, gui_receives)
        print("PASS: real GTK repeated outgoing/incoming transfer rows stay responsive", flush=True)


if __name__ == "__main__":
    if sys.platform != "darwin":
        raise SystemExit("This fixture uses macOS isolated HOME IPC sockets.")
    cluster = Cluster()
    try:
        run(cluster)
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        cluster.selector.close()
    for log in cluster.directory.glob("*/*.log"):
        text = log.read_text(errors="replace")
        assert "panicked at" not in text and "CRITICAL" not in text, log
    print("PASS: isolated fixtures shut down without panic or GTK critical", flush=True)
