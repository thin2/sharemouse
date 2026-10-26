"""Run native GTK assertions on the main thread with isolated preferences."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
if __name__ == "__main__":
    (ROOT / ".local").mkdir(exist_ok=True)
    environment = dict(os.environ)
    environment.update(
        RUSTUP_HOME=os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
        CARGO_HOME=os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
        LAN_MOUSE_DISABLE_AUTOSTART="1",
        LAN_MOUSE_RUN_UI_TEST="1",
    )
    for theme in ("light", "dark"):
        home = tempfile.mkdtemp(prefix="conversation-ui-", dir=ROOT / ".local")
        environment.update(HOME=home, XDG_CONFIG_HOME=str(Path(home) / ".config"), XDG_RUNTIME_DIR=home, LAN_MOUSE_UI_THEME=theme,
                           LAN_MOUSE_UI_SCREENSHOT=str(ROOT / f".local/conversation-{theme}.png"))
        result = subprocess.run(
            ["cargo", "test", "-p", "lan-mouse-gtk", "--test", "conversation_ui",
             "--locked", "--offline"], cwd=ROOT, env=environment,
        )
        if result.returncode:
            raise SystemExit(result.returncode)
