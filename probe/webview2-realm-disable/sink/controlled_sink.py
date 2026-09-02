#!/usr/bin/env python3
"""Bounded STUN/TURN UDP/TCP/TLS measurement sink.

This utility is deliberately credential-free and product-independent. It binds
one endpoint set per arm/realm lane, records only protocol counters and a
per-run nonce-bearing TURN username, and exits after a fixed duration.
"""
from __future__ import annotations

import argparse
import json
import socket
import ssl
import struct
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAGIC = b"\x21\x12\xa4\x42"
LANES = (
    "protected-initial",
    "protected-srcdoc",
    "control-initial",
    "control-srcdoc",
    "huddle",
)
TRANSPORTS = ("stun_udp", "turn_udp", "turn_tcp", "turns_tls")


def pad4(value: bytes) -> bytes:
    return value + b"\0" * ((4 - len(value) % 4) % 4)


def attribute(kind: int, value: bytes) -> bytes:
    return struct.pack("!HH", kind, len(value)) + pad4(value)


def parse_message(data: bytes) -> dict | None:
    if len(data) < 20 or data[4:8] != MAGIC:
        return None
    length = struct.unpack("!H", data[2:4])[0]
    if len(data) < 20 + length or length % 4:
        return None
    message_type = struct.unpack("!H", data[:2])[0]
    attrs: dict[int, list[bytes]] = {}
    cursor = 20
    while cursor + 4 <= 20 + length:
        kind, size = struct.unpack("!HH", data[cursor : cursor + 4])
        cursor += 4
        if cursor + size > len(data):
            return None
        attrs.setdefault(kind, []).append(data[cursor : cursor + size])
        cursor += (size + 3) & ~3
    return {
        "type": message_type,
        "transaction": data[8:20],
        "attrs": attrs,
        "wire_length": 20 + length,
    }


def stun_binding_success(request: bytes, peer: tuple[str, int]) -> bytes | None:
    parsed = parse_message(request)
    if not parsed or parsed["type"] != 0x0001:
        return None
    ip = socket.inet_aton(peer[0])
    port = peer[1] ^ 0x2112
    address = bytes(a ^ b for a, b in zip(ip, MAGIC))
    attrs = attribute(0x0020, b"\x00\x01" + struct.pack("!H", port) + address)
    return b"\x01\x01" + struct.pack("!H", len(attrs)) + MAGIC + parsed["transaction"] + attrs


def turn_challenge(request: bytes, realm: str, nonce: str) -> tuple[bytes, str | None] | None:
    parsed = parse_message(request)
    if not parsed or parsed["type"] != 0x0003:
        return None
    usernames = parsed["attrs"].get(0x0006, [])
    username = usernames[-1].decode("utf-8", "replace") if usernames else None
    error = b"\x00\x00\x04\x01Unauthorized"
    attrs = attribute(0x0009, error) + attribute(0x0014, realm.encode()) + attribute(0x0015, nonce.encode())
    response = b"\x01\x13" + struct.pack("!H", len(attrs)) + MAGIC + parsed["transaction"] + attrs
    return response, username


@dataclass
class BoundLane:
    stun_udp: socket.socket
    turn_udp: socket.socket
    turn_tcp: socket.socket
    turns_tls: socket.socket

    def endpoints(self) -> dict[str, int]:
        return {name: getattr(self, name).getsockname()[1] for name in TRANSPORTS}


class State:
    def __init__(self, token: str):
        self.token = token
        self.lock = threading.Lock()
        self.lanes = {
            lane: {
                transport: {"packets": 0, "valid": 0, "nonce_bound": 0}
                for transport in TRANSPORTS
            }
            for lane in LANES
        }

    def record(self, lane: str, transport: str, *, valid: bool, username: str | None = None) -> None:
        with self.lock:
            counter = self.lanes[lane][transport]
            counter["packets"] += 1
            if valid:
                counter["valid"] += 1
            if username and self.token in username and lane in username:
                counter["nonce_bound"] += 1

    def snapshot(self) -> dict:
        with self.lock:
            lanes = json.loads(json.dumps(self.lanes, sort_keys=True))
        return {
            "schema": "buzz-controlled-webrtc-sink-snapshot/v1",
            "token": self.token,
            "lanes": lanes,
        }


def bind_udp(host: str) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind((host, 0))
    return sock


def bind_tcp(host: str) -> socket.socket:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((host, 0))
    sock.listen(32)
    return sock


def serve_stun(sock: socket.socket, state: State, lane: str) -> None:
    while True:
        data, peer = sock.recvfrom(65535)
        reply = stun_binding_success(data, peer)
        state.record(lane, "stun_udp", valid=reply is not None)
        if reply:
            sock.sendto(reply, peer)


def serve_turn_udp(sock: socket.socket, state: State, lane: str) -> None:
    realm = "buzz-webview2-successor"
    nonce = f"nonce:{state.token}:{lane}"
    while True:
        data, peer = sock.recvfrom(65535)
        challenge = turn_challenge(data, realm, nonce)
        username = challenge[1] if challenge else None
        state.record(lane, "turn_udp", valid=challenge is not None, username=username)
        if challenge:
            sock.sendto(challenge[0], peer)


def read_stun(stream: socket.socket) -> bytes | None:
    stream.settimeout(5)
    head = b""
    while len(head) < 20:
        chunk = stream.recv(20 - len(head))
        if not chunk:
            return None
        head += chunk
    length = struct.unpack("!H", head[2:4])[0]
    body = b""
    while len(body) < length:
        chunk = stream.recv(length - len(body))
        if not chunk:
            return None
        body += chunk
    return head + body


def handle_turn_stream(stream: socket.socket, state: State, lane: str, transport: str) -> None:
    realm = "buzz-webview2-successor"
    nonce = f"nonce:{state.token}:{lane}"
    try:
        for _ in range(4):
            data = read_stun(stream)
            if not data:
                break
            challenge = turn_challenge(data, realm, nonce)
            username = challenge[1] if challenge else None
            state.record(lane, transport, valid=challenge is not None, username=username)
            if challenge:
                stream.sendall(challenge[0])
    except (OSError, ssl.SSLError):
        pass
    finally:
        try:
            stream.close()
        except OSError:
            pass


def serve_turn_tcp(listener: socket.socket, state: State, lane: str, transport: str, context: ssl.SSLContext | None = None) -> None:
    while True:
        stream, _ = listener.accept()
        if context:
            try:
                stream = context.wrap_socket(stream, server_side=True)
            except ssl.SSLError:
                stream.close()
                continue
        threading.Thread(
            target=handle_turn_stream,
            args=(stream, state, lane, transport),
            daemon=True,
        ).start()


def make_handler(state: State):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path != f"/snapshot/{state.token}":
                self.send_error(404)
                return
            body = (json.dumps(state.snapshot(), sort_keys=True) + "\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format, *args):
            del format, args
            return

    return Handler


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bind", required=True)
    parser.add_argument("--advertise", required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--duration-seconds", type=int, default=600)
    parser.add_argument("--cert", default=str(Path(__file__).with_name("measurement-cert.pem")))
    parser.add_argument("--key", default=str(Path(__file__).with_name("measurement-key.pem")))
    args = parser.parse_args()
    if len(args.token) < 24:
        parser.error("token must contain at least 24 characters")
    output = Path(args.output)
    if output.exists():
        raise SystemExit(f"refusing to overwrite endpoint evidence: {output}")

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.cert, args.key)
    state = State(args.token)
    bound: dict[str, BoundLane] = {}
    for lane in LANES:
        sockets = BoundLane(bind_udp(args.bind), bind_udp(args.bind), bind_tcp(args.bind), bind_tcp(args.bind))
        bound[lane] = sockets
        threading.Thread(target=serve_stun, args=(sockets.stun_udp, state, lane), daemon=True).start()
        threading.Thread(target=serve_turn_udp, args=(sockets.turn_udp, state, lane), daemon=True).start()
        threading.Thread(target=serve_turn_tcp, args=(sockets.turn_tcp, state, lane, "turn_tcp"), daemon=True).start()
        threading.Thread(target=serve_turn_tcp, args=(sockets.turns_tls, state, lane, "turns_tls", context), daemon=True).start()

    control = ThreadingHTTPServer((args.bind, 0), make_handler(state))
    threading.Thread(target=control.serve_forever, daemon=True).start()
    endpoints = {
        "schema": "buzz-controlled-webrtc-sink-endpoints/v1",
        "token": args.token,
        "advertised_host": args.advertise,
        "control_port": control.server_address[1],
        "lanes": {lane: sockets.endpoints() for lane, sockets in bound.items()},
    }
    output.write_text(json.dumps(endpoints, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"status": "READY", "output": str(output), "control_port": control.server_address[1]}, sort_keys=True), flush=True)
    deadline = time.monotonic() + args.duration_seconds
    while time.monotonic() < deadline:
        time.sleep(1)
    snapshot = output.with_name(output.stem + "-final-snapshot.json")
    snapshot.write_text(json.dumps(state.snapshot(), indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
