"""Use OS credential stores explicitly; never fall back to a plaintext key file."""

import sys
from functools import lru_cache

SERVICE = "io.github.matthehzhang.whisp"
ACCOUNT = "deepgram-api-key"


@lru_cache
def backend():
    if sys.platform == "win32":
        from keyring.backends.Windows import WinVaultKeyring

        return WinVaultKeyring()
    if sys.platform == "darwin":
        from keyring.backends.macOS import Keyring

        return Keyring()
    from keyring.backends.SecretService import Keyring

    return Keyring()


def read_key():
    try:
        return backend().get_password(SERVICE, ACCOUNT)
    except Exception:
        raise RuntimeError(
            "Cannot open the system credential store. Unlock it and restart Whisp."
        ) from None


def save_key(value):
    value = value.strip()
    if not value or any(ch.isspace() for ch in value):
        raise ValueError("Paste the API key alone, without spaces or the word Token.")
    try:
        backend().set_password(SERVICE, ACCOUNT, value)
    except Exception:
        raise RuntimeError("Could not save the key in the system credential store.") from None


def delete_key():
    from keyring.errors import PasswordDeleteError

    try:
        backend().delete_password(SERVICE, ACCOUNT)
    except PasswordDeleteError:
        pass
    except Exception:
        raise RuntimeError("Could not remove the key from the system credential store.") from None
