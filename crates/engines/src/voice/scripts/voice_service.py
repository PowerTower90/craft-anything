"""Craft Anything voice service: PLACEHOLDER (CRAFT-ANYTHING-VOICE-PLACEHOLDER).

This stand-in has the real service's command line and speaks its protocol (docs/voice-protocol.md)
with the Python standard library only, so the app's side can be started, connected and stopped
before the real service exists. It hears nothing and says nothing: push-to-talk ends without an
utterance, `speak` finishes at once without sound, and enrolment and voice design report that they
aren't available. The real service (microphone, VAD, Whisper, speaker check, Qwen3-TTS) replaces
this file.

    python -I voice_service.py --host 127.0.0.1 --port 8191 --models-dir <dir> [--device cuda:0]
           [--parent-pid <pid>]

It prints one line `READY {"port":8191,"version":1}` once it is listening, logs to stderr, serves
`ws://<host>:<port>/ws` (one client at a time; a new one replaces the old) and `GET /health`, and
stops on {"cmd":"shutdown"} or when the parent process is gone.
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import threading
import time

VERSION = 1
PLACEHOLDER = True
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_MESSAGE = 1 << 20


def log(msg):
    sys.stderr.write("voice_service: %s\n" % msg)
    sys.stderr.flush()


class State:
    def __init__(self, mode):
        self.lock = threading.Lock()
        self.client = None
        self.stop = threading.Event()
        self.listening = True
        self.capturing = False
        self.speaking = False
        self.mode = mode
        self.enrolled = False

    def snapshot(self):
        return {
            "listening": self.listening,
            "capturing": self.capturing,
            "speaking": self.speaking,
            "mode": self.mode,
            "enrolled": self.enrolled,
            "placeholder": PLACEHOLDER,
        }


def parent_alive(pid):
    if pid <= 0:
        return True
    if os.name == "nt":
        import ctypes

        kernel32 = ctypes.windll.kernel32
        handle = kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        if not handle:
            return False
        code = ctypes.c_ulong(0)
        ok = kernel32.GetExitCodeProcess(handle, ctypes.byref(code))
        kernel32.CloseHandle(handle)
        return bool(ok) and code.value == 259  # STILL_ACTIVE
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def watch_parent(pid, state):
    while not state.stop.is_set():
        if not parent_alive(pid):
            log("the parent process is gone; stopping")
            state.stop.set()
            return
        time.sleep(1.0)


def read_exact(conn, n):
    data = b""
    while len(data) < n:
        chunk = conn.recv(n - len(data))
        if not chunk:
            raise ConnectionError("closed")
        data += chunk
    return data


def recv_message(conn):
    """One text message from the client, or None when it closed."""
    parts = []
    while True:
        b0, b1 = read_exact(conn, 2)
        opcode = b0 & 0x0F
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack(">H", read_exact(conn, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", read_exact(conn, 8))[0]
        if length > MAX_MESSAGE:
            raise ConnectionError("message too large")
        mask = read_exact(conn, 4) if b1 & 0x80 else b"\0\0\0\0"
        payload = bytearray(read_exact(conn, length))
        for i in range(len(payload)):
            payload[i] ^= mask[i & 3]
        if opcode == 0x8:
            return None
        if opcode == 0x9:
            send_frame(conn, 0xA, bytes(payload))
            continue
        if opcode == 0xA:
            continue
        parts.append(bytes(payload))
        if b0 & 0x80:
            return b"".join(parts).decode("utf-8", "replace")


def send_frame(conn, opcode, payload):
    head = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        head += bytes([n])
    elif n < 65536:
        head += bytes([126]) + struct.pack(">H", n)
    else:
        head += bytes([127]) + struct.pack(">Q", n)
    conn.sendall(head + payload)


def send_event(state, conn, event):
    try:
        with state.lock:
            send_frame(conn, 0x1, json.dumps(event).encode("utf-8"))
    except OSError:
        pass


def send_state(state, conn):
    event = {"type": "state"}
    event.update(state.snapshot())
    send_event(state, conn, event)


def handle_command(state, conn, msg):
    cmd = msg.get("cmd")
    reply_id = msg.get("id")

    def reply(event):
        if reply_id is not None and "id" not in event:
            event["id"] = reply_id
        send_event(state, conn, event)

    if cmd == "config":
        mode = msg.get("listen_mode")
        if mode in ("push_to_talk", "wake_word", "always"):
            state.mode = mode
        reply({"type": "config_ok"})
        send_state(state, conn)
    elif cmd == "ptt":
        state.capturing = bool(msg.get("down"))
        send_state(state, conn)
    elif cmd == "speak":
        reply({"type": "spoken", "id": str(msg.get("id", "")), "interrupted": False})
    elif cmd == "stop_speaking":
        state.speaking = False
        send_state(state, conn)
    elif cmd == "enroll_start":
        reply({"type": "enroll_progress", "seconds": 0.0, "needed": 20.0})
    elif cmd == "enroll_stop":
        reply({"type": "enrolled", "ok": False})
        reply({"type": "error", "message": "Voice enrolment isn't available in the placeholder voice service.", "fatal": False})
    elif cmd == "design_voice":
        reply({"type": "error", "message": "Designing a voice isn't available in the placeholder voice service.", "fatal": False})
    elif cmd == "devices":
        reply({"type": "devices", "inputs": [], "outputs": []})
    elif cmd == "status":
        event = {"type": "status", "version": VERSION}
        event.update(state.snapshot())
        reply(event)
    elif cmd == "shutdown":
        log("shutdown asked")
        state.stop.set()
    else:
        reply({"type": "error", "message": "Unknown command: %s" % cmd, "fatal": False})


def serve_ws(state, conn):
    with state.lock:
        old = state.client
        state.client = conn
    if old is not None:
        try:
            old.close()
        except OSError:
            pass
    send_state(state, conn)
    try:
        while not state.stop.is_set():
            text = recv_message(conn)
            if text is None:
                break
            try:
                msg = json.loads(text)
            except ValueError:
                send_event(state, conn, {"type": "error", "message": "That wasn't JSON.", "fatal": False})
                continue
            if isinstance(msg, dict):
                handle_command(state, conn, msg)
    except (ConnectionError, OSError):
        pass
    finally:
        with state.lock:
            if state.client is conn:
                state.client = None
        try:
            conn.close()
        except OSError:
            pass


def handle(state, conn):
    try:
        conn.settimeout(10)
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = conn.recv(4096)
            if not chunk or len(head) > 65536:
                conn.close()
                return
            head += chunk
        lines = head.split(b"\r\n\r\n", 1)[0].decode("latin-1").split("\r\n")
        parts = lines[0].split(" ")
        path = parts[1] if len(parts) > 1 else "/"
        headers = {}
        for line in lines[1:]:
            if ":" in line:
                k, v = line.split(":", 1)
                headers[k.strip().lower()] = v.strip()
        if path == "/ws" and headers.get("upgrade", "").lower() == "websocket":
            key = headers.get("sec-websocket-key", "")
            accept = base64.b64encode(hashlib.sha1((key + WS_GUID).encode("ascii")).digest()).decode("ascii")
            conn.sendall(
                (
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    "Sec-WebSocket-Accept: %s\r\n\r\n" % accept
                ).encode("ascii")
            )
            conn.settimeout(None)
            serve_ws(state, conn)
            return
        if path == "/health":
            body = json.dumps({"ok": True, "version": VERSION, "state": state.snapshot()}).encode("utf-8")
            status = "200 OK"
        else:
            body = b'{"ok":false}'
            status = "404 Not Found"
        conn.sendall(
            ("HTTP/1.1 %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % (status, len(body))).encode(
                "ascii"
            )
            + body
        )
        conn.close()
    except (OSError, ValueError):
        try:
            conn.close()
        except OSError:
            pass


def main():
    p = argparse.ArgumentParser(description="Craft Anything voice service (placeholder)")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8191)
    p.add_argument("--models-dir", default="")
    p.add_argument("--device", default="cpu")
    p.add_argument("--parent-pid", type=int, default=0)
    p.add_argument("--listen-mode", default="push_to_talk")
    args = p.parse_args()

    state = State(args.listen_mode)
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.bind((args.host, args.port))
    server.listen(4)
    server.settimeout(0.25)
    if args.parent_pid:
        threading.Thread(target=watch_parent, args=(args.parent_pid, state), daemon=True).start()
    log("placeholder service on %s:%d (device %s, models in %s)" % (args.host, args.port, args.device, args.models_dir or "-"))
    sys.stdout.write("READY %s\n" % json.dumps({"port": args.port, "version": VERSION}, separators=(",", ":")))
    sys.stdout.flush()
    while not state.stop.is_set():
        try:
            conn, _ = server.accept()
        except socket.timeout:
            continue
        except OSError:
            break
        threading.Thread(target=handle, args=(state, conn), daemon=True).start()
    with state.lock:
        client = state.client
    if client is not None:
        try:
            send_frame(client, 0x8, b"")
            client.close()
        except OSError:
            pass
    server.close()
    log("stopped")


if __name__ == "__main__":
    main()
