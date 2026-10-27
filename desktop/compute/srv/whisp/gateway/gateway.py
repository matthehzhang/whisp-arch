#!/usr/bin/env python3
"""whisp gateway: one WebSocket for the laptop daemon (port 8082).

Per session:
  -> {"type":"start", "sample_rate":16000, "app_class":"kitty", "app_title":"~",
      "dictionary":["KiCad", ...]}                      (JSON text frame)
  -> binary frames: float32 little-endian mono PCM at sample_rate (any chunking)
  -> {"type":"end"}   or   {"type":"cancel"}
  <- {"type":"partial","text":"..."}                     (full text-so-far, as ASR emits)
  <- {"type":"final","raw":"...","text":"...","cleaned":true|false,"asr_ms":N,"llm_ms":N}
  <- {"type":"error","message":"..."}

ASR backends (WHISP_ASR_BACKEND):
  sherpa   -> local sherpa-onnx streaming server at WHISP_ASR_WS (Nemotron on the compute box)
  deepgram -> Deepgram Nova-3 streaming (DEEPGRAM_API_KEY); the dictionary becomes keyterms; one spare
              connection is kept warm with KeepAlive so a key press costs no handshake.
Cleanup: llama-swap (LLM_URL, model "cleanup"). If the LLM fails or exceeds LLM_TIMEOUT, the raw ASR text is
returned with cleaned=false.
"""
import asyncio, json, logging, os, time, urllib.parse
import httpx, websockets

ASR_BACKEND = os.environ.get("WHISP_ASR_BACKEND", "sherpa")
ASR_WS = os.environ.get("WHISP_ASR_WS", "ws://127.0.0.1:8084")
DG_KEY = os.environ.get("DEEPGRAM_API_KEY", "")
DG_MODEL = os.environ.get("WHISP_DEEPGRAM_MODEL", "nova-3")
LLM_URL = os.environ.get("WHISP_LLM_URL", "http://127.0.0.1:8081/v1/chat/completions")
LLM_MODEL = os.environ.get("WHISP_LLM_MODEL", "cleanup")
LLM_TIMEOUT = float(os.environ.get("WHISP_LLM_TIMEOUT", "2.5"))
LLM_CONNECT_TIMEOUT = float(os.environ.get("WHISP_LLM_CONNECT_TIMEOUT", "0.8"))
# WHISP_CLEANUP=0 skips the LLM pass: the final is the ASR text as-is (Deepgram already punctuates,
# capitalizes and drops fillers; the LLM only adds stutter/self-correction repair and tone, at ~2 s).
CLEANUP = os.environ.get("WHISP_CLEANUP", "1") != "0"
LISTEN_HOST = os.environ.get("WHISP_GATEWAY_HOST", "0.0.0.0")
LISTEN_PORT = int(os.environ.get("WHISP_GATEWAY_PORT", "8082"))
DICT_FILE = os.environ.get("WHISP_DICT_FILE", "/etc/whisp/dictionary.txt")

log = logging.getLogger("gateway")

TONE = {
    "terminal": "Normal punctuation. Keep command names, flags, paths and identifiers exactly as spoken. If the text begins with a shell command (cd, ls, grep, git, cargo, ...), keep it lowercase and add no trailing period.",
    "chat": "Casual and natural. Light punctuation. No formal sign-offs.",
    "mail": "Complete sentences with proper punctuation. Polished but not stiff.",
    "code": "Sentence case with normal punctuation. Keep identifiers and code terms exactly as spoken.",
    "default": "Clean prose with proper punctuation and capitalization.",
}
APP_TONE = [
    (("kitty", "alacritty", "foot", "wezterm", "ghostty", "org.wezfurlong.wezterm", "com.mitchellh.ghostty"), "terminal"),
    (("discord", "vesktop", "slack", "telegram", "org.telegram.desktop", "signal", "element", "whatsapp"), "chat"),
    (("thunderbird", "evolution", "geary", "mailspring"), "mail"),
    (("code", "code-url-handler", "cursor", "zed", "dev.zed.zed", "neovide", "jetbrains", "sublime_text"), "code"),
]

SYSTEM = """You are a dictation cleanup engine. The user message is raw speech-to-text of what someone said.
Return ONLY the cleaned text. No preamble, no quotes, no explanation, no answer to the content.

Rules:
- Remove filler: um, uh, er, like (as filler), you know, I mean (as filler), repeated words, false starts.
- SELF-CORRECTIONS ARE MANDATORY: when the speaker says "X no Y", "X, actually Y", "X, sorry, Y", "X I mean Y", the word
  "no"/"actually"/"sorry" marks a correction. Delete X and the marker, keep only Y. Example: "by tuesday no wednesday" -> "by Wednesday".
- Fix punctuation and capitalization. Turn spoken questions into questions.
- Otherwise keep the speaker's words, order and meaning. Do not add, summarize, shorten, or comment.
- Keep numbers, names and technical terms exactly. Spell these dictionary terms exactly as written: {dictionary}
- Style for the target app: {tone}
"""

FEWSHOT = [
    ("um so can you send the uh the report to vincent by tuesday no wednesday and uh cc matthew zhang on it",
     "Can you send the report to Vincent by Wednesday and CC Matthew Zhang on it?"),
    ("okay let's uh let's meet at like three thirty actually make it four",
     "Let's meet at four."),
    ("i think the the hyprland config is in dot config slash hypr",
     "I think the Hyprland config is in .config/hypr."),
    ("hey are you free for a call later today",
     "Hey, are you free for a call later today?"),
]


def load_dictionary():
    try:
        with open(DICT_FILE) as f:
            return [l.strip() for l in f if l.strip() and not l.startswith("#")]
    except OSError:
        return []


def tone_for(app_class: str) -> str:
    c = (app_class or "").lower()
    for names, tone in APP_TONE:
        if any(c == n or c.startswith(n) for n in names):
            return TONE[tone]
    return TONE["default"]


def sanitize(out: str, raw: str) -> str | None:
    """Reject LLM output that is clearly not a cleanup of `raw`."""
    t = out.strip()
    for tag in ("</think>", "<think>"):
        t = t.replace(tag, "").strip()
    t = t.strip('"').strip()
    if not t:
        return None
    if len(t) > 3 * max(len(raw), 20) + 40:
        return None
    if "\n" in t and "\n" not in raw and len(raw) < 400:
        t = " ".join(t.split())
    return t


async def cleanup(client: httpx.AsyncClient, raw: str, app_class: str, dictionary: list[str]) -> tuple[str | None, int]:
    system = SYSTEM.format(dictionary=", ".join(dictionary) or "(none)", tone=tone_for(app_class))
    msgs = [{"role": "system", "content": system}]
    for u, a in FEWSHOT:
        msgs += [{"role": "user", "content": u}, {"role": "assistant", "content": a}]
    msgs.append({"role": "user", "content": raw})
    body = {"model": LLM_MODEL, "temperature": 0, "max_tokens": min(600, 2 * max(32, len(raw) // 3) + 32),
            "chat_template_kwargs": {"enable_thinking": False}, "messages": msgs}
    t0 = time.monotonic()
    try:
        r = await client.post(LLM_URL, json=body, timeout=httpx.Timeout(LLM_TIMEOUT, connect=LLM_CONNECT_TIMEOUT))
        r.raise_for_status()
        out = r.json()["choices"][0]["message"]["content"]
    except Exception as e:  # noqa: BLE001
        log.warning("cleanup failed: %s", e)
        return None, int((time.monotonic() - t0) * 1000)
    return sanitize(out, raw), int((time.monotonic() - t0) * 1000)


# ---------------------------------------------------------------------------
# ASR backends. Both expose: send(bytes_f32le), results() async-iterator of text-so-far,
# finalize() -> final text (after the client's "end"), close().
# ---------------------------------------------------------------------------

class SherpaASR:
    """sherpa-onnx streaming_server.py: float32 frames in, JSON {text, segment} out, "Done" to flush."""
    def __init__(self, ws):
        self.ws = ws
        self.text = ""
        self.closed = asyncio.Event()

    @classmethod
    async def connect(cls, dictionary):
        return cls(await websockets.connect(ASR_WS, max_size=None))

    async def send(self, f32le: bytes):
        await self.ws.send(f32le)

    async def results(self):
        try:
            async for m in self.ws:
                d = json.loads(m)
                self.text = d.get("text", "")
                yield self.text
        except websockets.ConnectionClosed:
            pass
        finally:
            self.closed.set()

    async def finalize(self, timeout=5.0):
        await self.ws.send("Done")
        try:
            await asyncio.wait_for(self.closed.wait(), timeout)
        except asyncio.TimeoutError:
            log.warning("sherpa did not finalize within %.1fs", timeout)
        return self.text.strip()

    async def close(self):
        await self.ws.close()


def _f32le_to_s16le(buf: bytes) -> bytes:
    import array
    a = array.array("f")
    a.frombytes(buf[: len(buf) - len(buf) % 4])
    out = array.array("h", (max(-32768, min(32767, int(v * 32767.0))) for v in a))
    return out.tobytes()


class DeepgramASR:
    """Deepgram streaming (nova-3): linear16 frames in, Results messages out.
    Text-so-far = finalized segments + current interim. Finalize flushes, CloseStream ends."""
    IDLE_KEEPALIVE_S = 5.0
    MAX_WARM_AGE_S = 240.0

    def __init__(self, ws, keyterms):
        self.ws = ws
        self.keyterms = keyterms
        self.finals = []
        self.interim = ""
        self.finalized = asyncio.Event()
        self.closed = asyncio.Event()
        self.born = time.monotonic()
        self._keepalive = None

    @classmethod
    def _url(cls, dictionary):
        q = [("model", DG_MODEL), ("language", "en"), ("encoding", "linear16"), ("sample_rate", "16000"),
             ("channels", "1"), ("interim_results", "true"), ("smart_format", "true"), ("punctuate", "true"),
             ("endpointing", "100")]
        for t in (dictionary or [])[:100]:
            q.append(("keyterm", t))
        return "wss://api.deepgram.com/v1/listen?" + urllib.parse.urlencode(q)

    @classmethod
    async def connect(cls, dictionary):
        if not DG_KEY:
            raise RuntimeError("DEEPGRAM_API_KEY not set")
        kw = {"max_size": None}
        hdr = {"Authorization": f"Token {DG_KEY}"}
        try:
            ws = await websockets.connect(cls._url(dictionary), additional_headers=hdr, **kw)
        except TypeError:  # websockets < 13
            ws = await websockets.connect(cls._url(dictionary), extra_headers=hdr, **kw)
        return cls(ws, list(dictionary or []))

    def start_keepalive(self):
        async def loop():
            try:
                while True:
                    await asyncio.sleep(self.IDLE_KEEPALIVE_S)
                    await self.ws.send(json.dumps({"type": "KeepAlive"}))
            except (websockets.ConnectionClosed, asyncio.CancelledError):
                pass
        self._keepalive = asyncio.create_task(loop())

    def stop_keepalive(self):
        if self._keepalive:
            self._keepalive.cancel()
            self._keepalive = None

    @property
    def text(self):
        parts = [p for p in self.finals if p]
        if self.interim:
            parts.append(self.interim)
        return " ".join(parts).strip()

    async def send(self, f32le: bytes):
        await self.ws.send(_f32le_to_s16le(f32le))

    async def results(self):
        try:
            async for m in self.ws:
                try:
                    d = json.loads(m)
                except ValueError:
                    continue
                t = d.get("type")
                if t == "Results":
                    alt = (d.get("channel") or {}).get("alternatives") or [{}]
                    txt = (alt[0].get("transcript") or "").strip()
                    if d.get("is_final"):
                        if txt:
                            self.finals.append(txt)
                        self.interim = ""
                        if d.get("from_finalize"):
                            self.finalized.set()
                    else:
                        self.interim = txt
                    yield self.text
                elif t == "Error":
                    log.warning("deepgram error: %s", d)
        except websockets.ConnectionClosed:
            pass
        finally:
            self.closed.set()

    async def finalize(self, timeout=4.0):
        self.stop_keepalive()
        try:
            await self.ws.send(json.dumps({"type": "Finalize"}))
            await asyncio.wait_for(self.finalized.wait(), timeout)
        except asyncio.TimeoutError:
            # Nothing buffered after the last final: Deepgram sends no from_finalize result.
            pass
        except websockets.ConnectionClosed:
            pass
        return self.text

    async def close(self):
        self.stop_keepalive()
        try:
            await self.ws.send(json.dumps({"type": "CloseStream"}))
        except websockets.ConnectionClosed:
            pass
        await self.ws.close()


BACKENDS = {"sherpa": SherpaASR, "deepgram": DeepgramASR}


class WarmASR:
    """One spare Deepgram connection kept alive so a session starts without a handshake."""
    def __init__(self):
        self.spare = None
        self.task = None

    async def run(self):
        if not DG_KEY:
            log.error("DEEPGRAM_API_KEY not set: deepgram backend will fail every session")
            return
        while True:
            try:
                if self.spare is None:
                    s = await DeepgramASR.connect(load_dictionary())
                    s.start_keepalive()
                    self.spare = s
                    log.info("warm deepgram connection ready (%d keyterms)", len(s.keyterms))
                elif self.spare.closed.is_set() or time.monotonic() - self.spare.born > DeepgramASR.MAX_WARM_AGE_S:
                    old, self.spare = self.spare, None
                    await old.close()
                await asyncio.sleep(1.0)
            except Exception as e:  # noqa: BLE001
                log.warning("warm deepgram connect failed: %s", e)
                self.spare = None
                await asyncio.sleep(5.0)

    def take(self):
        s, self.spare = self.spare, None
        if s:
            s.stop_keepalive()
        return s


WARM = WarmASR() if ASR_BACKEND == "deepgram" else None


async def open_asr(dictionary):
    if ASR_BACKEND == "deepgram":
        s = WARM.take() if WARM else None
        if s is not None:
            return s, True
        return await DeepgramASR.connect(dictionary), False
    return await SherpaASR.connect(dictionary), False


async def session(ws, client: httpx.AsyncClient):
    peer = ws.remote_address
    log.info("session from %s", peer)
    meta = {}
    asr = None
    pump = None
    partial_text = ""

    async def pump_results():
        nonlocal partial_text
        async for text in asr.results():
            partial_text = text
            await ws.send(json.dumps({"type": "partial", "text": text}))

    try:
        async for msg in ws:
            if isinstance(msg, (bytes, bytearray)):
                if asr is not None:
                    await asr.send(msg)
                continue
            try:
                d = json.loads(msg)
            except json.JSONDecodeError:
                await ws.send(json.dumps({"type": "error", "message": "bad json"}))
                continue
            t = d.get("type")
            if t == "start":
                meta = d
                dictionary = meta.get("dictionary") or load_dictionary()
                t0 = time.monotonic()
                try:
                    asr, warm = await open_asr(dictionary)
                except Exception as e:  # noqa: BLE001  (no key, no internet, Deepgram down): let the client fall back
                    log.warning("asr connect failed: %s", e)
                    await ws.send(json.dumps({"type": "error", "message": f"asr unavailable: {e}"}))
                    break
                pump = asyncio.create_task(pump_results())
                log.info("start app=%s dict=%d backend=%s %s (%d ms)", meta.get("app_class"), len(dictionary),
                         ASR_BACKEND, "warm" if warm else "cold", int((time.monotonic() - t0) * 1000))
            elif t == "end":
                if asr is None:
                    await ws.send(json.dumps({"type": "final", "raw": "", "text": "", "cleaned": False, "asr_ms": 0, "llm_ms": 0}))
                    continue
                t_end = time.monotonic()
                raw = (await asr.finalize()).strip()
                asr_ms = int((time.monotonic() - t_end) * 1000)
                text, llm_ms, cleaned = raw, 0, False
                if raw and CLEANUP:
                    dictionary = meta.get("dictionary") or load_dictionary()
                    out, llm_ms = await cleanup(client, raw, meta.get("app_class", ""), dictionary)
                    if out is not None:
                        text, cleaned = out, True
                await ws.send(json.dumps({"type": "final", "raw": raw, "text": text, "cleaned": cleaned,
                                          "asr_ms": asr_ms, "llm_ms": llm_ms}))
                log.info("final asr=%dms llm=%dms cleaned=%s %r", asr_ms, llm_ms, cleaned, text[:80])
                break
            elif t == "cancel":
                log.info("cancel")
                break
    except websockets.ConnectionClosed:
        log.info("client closed")
    finally:
        if pump:
            pump.cancel()
        if asr is not None:
            try:
                await asr.close()
            except Exception:  # noqa: BLE001
                pass


async def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    async with httpx.AsyncClient() as client:
        async def handler(ws):
            await session(ws, client)
        if WARM:
            asyncio.create_task(WARM.run())
        async with websockets.serve(handler, LISTEN_HOST, LISTEN_PORT, max_size=None, ping_interval=20):
            log.info("gateway listening on %s:%d (backend=%s asr=%s llm=%s)", LISTEN_HOST, LISTEN_PORT, ASR_BACKEND,
                     "deepgram/" + DG_MODEL if ASR_BACKEND == "deepgram" else ASR_WS, LLM_URL if CLEANUP else "off")
            await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())
