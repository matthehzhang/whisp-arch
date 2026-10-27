"""One listener per user; reopening the app brings up the running settings window."""

import os
import socket
import threading


class Instance:
    def __init__(self, directory):
        self.directory = directory
        self.path = directory / "instance.sock"
        self.file = None
        self.server = None

    def acquire(self):
        import fcntl

        self.directory.mkdir(parents=True, exist_ok=True)
        self.file = (self.directory / "instance.lock").open("a+")
        try:
            fcntl.flock(self.file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.file.close()
            self.file = None
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(2)
                try:
                    client.connect(str(self.path))
                    client.sendall(b"show")
                except OSError:
                    pass
            return False
        self.path.unlink(missing_ok=True)
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(str(self.path))
        os.chmod(self.path, 0o600)
        self.server.listen(2)
        self.server.settimeout(0.5)
        return True

    def listen(self, show):
        def run():
            server = self.server
            while self.server:
                try:
                    client, _ = server.accept()
                    with client:
                        client.settimeout(1)
                        if client.recv(16) == b"show":
                            show()
                except socket.timeout:
                    continue
                except OSError:
                    break

        threading.Thread(target=run, daemon=True).start()

    def close(self):
        server, self.server = self.server, None
        if server:
            server.close()
            self.path.unlink(missing_ok=True)
        if self.file:
            self.file.close()
            self.file = None
