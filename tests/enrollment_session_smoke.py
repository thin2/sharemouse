"""Exercise IP-only enrollment using isolated production daemons on macOS."""
import socket
import sys

from sharing_session_smoke import Cluster


def change_port(cluster, node):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    cluster.send(node, {"ChangePort": port})
    cluster.wait(lambda: node["events"].get("PortChanged", [0])[0] == port,
                 "input listener changed")
    return port


def clients(cluster, node):
    node["events"].pop("Enumerate", None)
    cluster.send(node, {"Enumerate": []})
    cluster.wait(lambda: "Enumerate" in node["events"], "fresh client list")
    return node["events"]["Enumerate"]


def peer_state(cluster, node, peer):
    fingerprint = peer["events"]["PublicKeyFingerprint"]
    return next(row for row in clients(cluster, node)
                if row[2].get("peer_fingerprint") == fingerprint)


class EnrollmentCluster(Cluster):
    def pair(self, host, peer, address, approve=True, full=False, change_during_pairing=False):
        for node in (host, peer):
            node["events"].pop("EnrollmentApproval", None)
        self.send(peer, {"JoinGroup": {"address": address}})
        self.wait(lambda: all("EnrollmentApproval" in n["events"] for n in (host, peer)),
                  "both screens request confirmation")
        if change_during_pairing:
            change_port(self, host)
        for node, other in ((host, peer), (peer, host)):
            approval = node["events"]["EnrollmentApproval"]
            assert approval["fingerprint"] == other["events"]["PublicKeyFingerprint"]
            self.send(node, {"ConfirmEnrollment": {"id": approval["id"],
                                                   "approve": approve or node is peer}})
        self.wait(lambda: peer["events"].get("EnrollmentState", {}).get("status") ==
                  ("joined" if approve and not full and not change_during_pairing else "failed"),
                  "pairing result")

    def run(self):
        a, b, c = [self.start(name) for name in ("a", "b", "c")]
        # Use distinct real UDP ports for each isolated daemon.
        for node in self.nodes:
            change_port(self, node)
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as occupied:
            occupied.bind(("0.0.0.0", 0))
            occupied.listen()
            self.send(a, {"HostGroup": {"port": occupied.getsockname()[1]}})
            self.wait(lambda: a["events"].get("EnrollmentState", {}).get("status") == "failed",
                      "occupied joining port reports failure instead of stale hosting")
        self.send(a, {"HostGroup": {"port": 0}})
        self.wait(lambda: a["events"].get("EnrollmentState", {}).get("status") == "hosting",
                  "main device allows joining")
        shown = a["events"]["EnrollmentState"]["addresses"][0]
        address = "127.0.0.1:" + shown.rsplit(":", 1)[1]
        self.pair(a, b, address, approve=False)
        assert not a["events"].get("AuthorizedUpdated")
        assert not b["events"].get("AuthorizedUpdated")
        print("PASS: rejection grants no authorization", flush=True)
        self.pair(a, b, address, change_during_pairing=True)
        assert not a["events"].get("AuthorizedUpdated")
        assert not b["events"].get("AuthorizedUpdated")
        print("PASS: port change during confirmation fails before granting trust", flush=True)
        # The accept loop must read current ports, not its startup snapshot.
        host_port = change_port(self, a)
        self.pair(a, b, address)
        assert peer_state(self, b, a)[1]["port"] == host_port
        handle = peer_state(self, a, b)[0]
        self.send(a, {"Activate": [handle, False]})
        self.send(a, "SaveConfiguration")
        assert not peer_state(self, a, b)[2]["active"]
        self.pair(a, c, address)
        self.wait(lambda: all(len(n["events"].get("AuthorizedUpdated", {})) == 2
                             for n in self.nodes), "third member propagated")
        self.wait(lambda: self.all_phase("Stopped"), "full mesh confirms stop", seconds=25)
        assert not peer_state(self, a, b)[2]["active"], "third join reactivated disabled peer"
        follower_port = change_port(self, b)
        self.wait(lambda: peer_state(self, a, b)[1]["port"] == follower_port
                  and peer_state(self, c, b)[1]["port"] == follower_port,
                  "follower port update propagated")
        assert not peer_state(self, a, b)[2]["active"], "port update reactivated disabled peer"
        print("PASS: live host/follower input ports propagate; disabled peer stays disabled", flush=True)
        self.send(b, "StartSharing")
        self.wait(lambda: self.all_phase("Running", b["events"]["PublicKeyFingerprint"]),
                  "joined peer can be keyboard primary")
        self.send(b, "StopSharing")
        self.wait(lambda: self.all_phase("Stopped"), "stop joined group")
        print("PASS: only main IP entered; three members share without manual authorization", flush=True)
        d = self.start("d")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.send(d, {"ChangePort": port})
        self.wait(lambda: d["events"].get("PortChanged", [0])[0] == port, "fourth input port")
        self.pair(a, d, address, full=True)
        assert not d["events"].get("AuthorizedUpdated")
        self.stop(d)
        print("PASS: fourth device rejected without disturbing the group", flush=True)
        self.stop(b)
        b = self.start("b", existing=b)
        assert b["events"]["PortChanged"][0] == follower_port
        self.wait(lambda: self.all_phase("Stopped"), "persisted group reconnects", seconds=25)
        print("PASS: restart restores full group and endpoints", flush=True)
        if "--gtk" in sys.argv:
            self.gtk_smoke(a)
        fp = c["events"]["PublicKeyFingerprint"]
        self.send(a, {"RemoveAuthorizedKey": fp})
        self.wait(lambda: all(fp not in n["events"].get("AuthorizedUpdated", {})
                             for n in (a, b)), "host revocation propagates")
        self.stop(a)
        a = self.start("a", existing=a)
        assert fp not in a["events"].get("AuthorizedUpdated", {})
        assert len(clients(self, a)) == 1, "restoration duplicated configured peers"
        assert not peer_state(self, a, b)[2]["active"], "restart reactivated disabled peer"
        print("PASS: revocation persists after restart", flush=True)


if __name__ == "__main__":
    cluster = EnrollmentCluster()
    try:
        cluster.run()
    finally:
        for node in cluster.nodes[:]:
            cluster.stop(node)
        print(f"Logs: {cluster.directory}", flush=True)
