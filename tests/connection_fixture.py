"""Shared isolated three-device TLS/DTLS fixture for integration tests."""
import socket
import time


def configure(cluster):
    a, b, c = [cluster.start(n) for n in ("a", "b", "c")]
    for node in cluster.nodes:
        for peer in cluster.nodes:
            if node is not peer:
                cluster.send(node, {"AuthorizeKey": [peer["name"], peer["events"]["PublicKeyFingerprint"]]})
    cluster.wait(lambda: all(len(n["events"].get("AuthorizedUpdated", {})) == 2 for n in cluster.nodes),
                 "authorized complete group")
    cluster.connect_all()
    cluster.wait(lambda: cluster.all_phase("Stopped"), "initial stop")
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as port_socket:
        port_socket.bind(("127.0.0.1", 0))
        input_port = port_socket.getsockname()[1]
    cluster.send(b, {"ChangePort": input_port})
    cluster.wait(lambda: b["events"].get("PortChanged", [0])[0] > 0, "input listener")
    cluster.send(a, "Create")
    cluster.wait(lambda: "Created" in a["events"], "create configured recipient")
    handle = a["events"]["Created"][0]
    for request in [
        {"UpdateHostname": [handle, "127.0.0.1"]},
        {"UpdateFixIps": [handle, ["127.0.0.1"]]},
        {"UpdatePort": [handle, b["events"]["PortChanged"][0]]},
        {"UpdatePosition": [handle, "left"]},
        {"Activate": [handle, True]},
        "EnableCapture",
    ]:
        cluster.send(a, request)
    cluster.send(a, "SaveConfiguration")
    config_a = cluster.directory / "a" / "config.toml"
    cluster.wait(lambda: "activate_on_startup = true" in config_a.read_text(), "save active fixture")
    cluster.stop(a)
    a = cluster.start("a", existing=a)
    cluster.connect_all()
    deadline = time.monotonic() + 12
    while True:
        cluster.send(a, {"Enumerate": []})
        cluster.pump()
        clients = a["events"].get("Enumerate", [])
        if any(h == handle and s.get("peer_fingerprint") == b["events"]["PublicKeyFingerprint"]
               for h, _, s in clients):
            break
        assert time.monotonic() < deadline, "DTLS did not learn recipient identity"
    cluster.send(a, {"Activate": [handle, False]})
    cluster.send(a, "SaveConfiguration")
    return a, b, c, handle
