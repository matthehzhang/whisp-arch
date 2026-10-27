#!/usr/bin/env python3
"""Drive the overlay without the daemon: bind the events socket and play a scripted dictation."""
import json, math, os, socket, sys, time
path = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "whisp-arch.events")
try: os.unlink(path)
except FileNotFoundError: pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); srv.bind(path); srv.listen(4)
print("fake daemon listening; waiting for overlay...", flush=True)
conn, _ = srv.accept()
def send(d): conn.sendall((json.dumps(d) + "\n").encode())
words = "so the kicad footprint for the buck converter is wrong it should be 0603 not 0402".split()
hold = float(sys.argv[1]) if len(sys.argv) > 1 else 6.0
time.sleep(1.0)
send({"ev": "state", "state": "recording"})
t0 = time.time(); i = 0
while time.time() - t0 < hold:
    t = time.time() - t0
    rms = 0.03 + 0.12 * abs(math.sin(t * 2.3)) * (0.5 + 0.5 * abs(math.sin(t * 7.1)))
    bands = [max(0.0, min(1.0, rms/0.15 * (0.4 + 0.6*abs(math.sin(t*3.1 + k*0.8))))) for k in range(8)]
    send({"ev": "level", "rms": rms, "bands": bands})
    if int(t / (hold / len(words))) > i and i < len(words):
        i += 1; send({"ev": "partial", "text": " ".join(words[:i])})
    time.sleep(0.04)
send({"ev": "state", "state": "transcribing"}); time.sleep(0.9)
send({"ev": "final", "text": "The KiCad footprint for the buck converter is wrong; it should be 0603, not 0402.", "raw": " ".join(words), "cleaned": True, "asr_ms": 140, "llm_ms": 180})
time.sleep(0.3); send({"ev": "state", "state": "ready"}); time.sleep(2.0)
print("done", flush=True)
