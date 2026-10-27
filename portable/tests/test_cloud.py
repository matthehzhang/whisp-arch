import io
import wave

import numpy as np
import pytest
import requests

from whisp.transcribe import ENDPOINT, Engine


class Response:
    def __init__(self, status=200, data=None):
        self.status_code = status
        self.data = (
            data
            if data is not None
            else {"results": {"channels": [{"alternatives": [{"transcript": "Hello  world."}]}]}}
        )
        self.closed = False

    def json(self):
        return self.data

    def close(self):
        self.closed = True


def test_upload_is_mono_wav_over_https_with_key_header():
    response = Response()
    calls = []

    def post(url, **kwargs):
        calls.append((url, kwargs))
        return response

    engine = Engine(key_reader=lambda: "test-key", post=post)
    assert engine.transcribe(np.ones(16000, np.float32) * 0.2) == "Hello world."
    assert len(calls) == 1
    url, options = calls[0]
    assert url == ENDPOINT and url.startswith("https://")
    assert options["headers"]["Authorization"] == "Token test-key"
    assert options["allow_redirects"] is False
    assert options["timeout"] == (5, 60)
    assert options["params"] == {"model": "nova-3", "smart_format": "true", "language": "multi"}
    with wave.open(io.BytesIO(options["data"])) as wav:
        assert (wav.getnchannels(), wav.getframerate(), wav.getsampwidth()) == (1, 16000, 2)
        assert wav.getnframes() == 16000
    assert response.closed


@pytest.mark.parametrize(
    "status,message",
    [
        (401, "API key"),
        (403, "API key"),
        (402, "credit"),
        (429, "limit"),
        (500, "HTTP 500"),
        (302, "HTTP 302"),
    ],
)
def test_service_errors_are_actionable_and_not_retried(status, message):
    calls = []
    response = Response(status)

    def post(*args, **kwargs):
        calls.append(True)
        return response

    engine = Engine(key_reader=lambda: "do-not-log-this", post=post)
    with pytest.raises(RuntimeError, match=message) as error:
        engine.transcribe(np.ones(16000, np.float32))
    assert "do-not-log-this" not in str(error.value)
    assert len(calls) == 1
    assert response.closed


@pytest.mark.parametrize("exception", [requests.Timeout, requests.ConnectionError])
def test_network_errors_hide_request_details(exception):
    def fail(*args, **kwargs):
        raise exception("private request details")

    engine = Engine(key_reader=lambda: "key", post=fail)
    with pytest.raises(RuntimeError) as error:
        engine.transcribe(np.ones(16000, np.float32))
    assert "private request" not in str(error.value)


def test_missing_key_never_uploads():
    def fail(*args, **kwargs):
        raise AssertionError("must not upload")

    engine = Engine(key_reader=lambda: None, post=fail)
    with pytest.raises(RuntimeError, match="API key"):
        engine.transcribe(np.ones(16000, np.float32))


def test_cancel_while_retrieving_key_prevents_upload():
    cancelled = False

    def read_key():
        nonlocal cancelled
        cancelled = True
        return "key"

    def fail(*args, **kwargs):
        raise AssertionError("Cancelled audio must not be uploaded")

    engine = Engine(key_reader=read_key, post=fail)
    assert engine.transcribe(np.ones(16000, np.float32), lambda: cancelled) == ""


def test_malformed_response_is_rejected_and_closed():
    response = Response(data={"results": []})
    engine = Engine(key_reader=lambda: "key", post=lambda *args, **kwargs: response)
    with pytest.raises(RuntimeError, match="unreadable"):
        engine.transcribe(np.ones(16000, np.float32))
    assert response.closed
