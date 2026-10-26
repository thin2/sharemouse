"""macOS named-pasteboard regression: sync works without persisted history."""
import sys

from sharing_session_smoke import Cluster
from connection_fixture import configure
from image_clipboard_session_smoke import ImageBoards, animated_gif


def main():
    if sys.platform != "darwin":
        raise SystemExit("Requires macOS named NSPasteboards.")
    cluster = Cluster()
    boards = ImageBoards(cluster)
    try:
        a, b, c, _ = configure(cluster)
        legacy = cluster.directory / "a" / "clipboard-history"
        legacy.mkdir()
        sentinel = legacy / "index.json"
        sentinel.write_bytes(b"legacy history must not be read or overwritten")

        def handover(node):
            if not cluster.all_phase("Stopped"):
                cluster.send(node, "StopSharing")
                cluster.wait(lambda: cluster.all_phase("Stopped"), "stop barrier")
            cluster.send(node, "StartSharing")
            cluster.wait(lambda: cluster.all_phase("Running", node["events"]["PublicKeyFingerprint"]),
                         "new primary")

        handover(b)
        for n in range(3):
            text = f"live clipboard without history {n}"
            boards.write(a, text)
            cluster.wait(lambda: boards.read(b) == text, "text synchronized")
        handover(c)
        cluster.wait(lambda: boards.read(c) == text, "latest text follows target")
        gif = animated_gif()
        fixture = cluster.directory / "fixture.gif"
        fixture.write_bytes(gif)
        boards.write_files(a, [fixture])
        cluster.wait(lambda: boards.original_matches(c, gif), "image synchronized")
        for node in cluster.nodes:
            cluster.send(node, "ListClipboardHistory")
        cluster.wait(lambda: all(n["events"].get("ClipboardHistory") == [] for n in cluster.nodes),
                     "legacy IPC returns no history")
        cluster.send(a, "StopSharing")
        cluster.wait(lambda: cluster.all_phase("Stopped"), "stop before restart")
        old_serial = boards.serial(a)
        cluster.stop(a)
        a = cluster.start("a", existing=a)
        cluster.send(a, "ListClipboardHistory")
        cluster.wait(lambda: a["events"].get("ClipboardHistory") == [], "restart without history")
        assert boards.serial(a) == old_serial
        assert sentinel.read_bytes() == b"legacy history must not be read or overwritten"
        assert list(legacy.iterdir()) == [sentinel]
        for name in ("b", "c"):
            assert not (cluster.directory / name / "clipboard-history").exists()
        assert all(not node["errors"] for node in cluster.nodes)
        print("PASS: live text/image sync, target handover and restart without history")
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        boards.close()
        cluster.selector.close()


if __name__ == "__main__":
    main()
