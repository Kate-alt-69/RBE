"""Portable Library Protocol worker loop for RBE Python packages."""

from __future__ import annotations

import asyncio
import inspect
import json
import struct
import sys
from dataclasses import dataclass
from typing import Any, BinaryIO, Callable

from . import LIBRARY_ABI_VERSION, SDK_VERSION, HostCall, HostSessionInfo, RbeSdk

LIBRARY_PROTOCOL_VERSION = 1
MAX_LIBRARY_FRAME_BYTES = 8 * 1024 * 1024
MAX_LIBRARY_PAYLOAD_BYTES = 2 * 1024 * 1024


class LibraryProtocolError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class LibraryWorkerIdentity:
    package_name: str
    package_version: str
    artifact_sha256: str
    runtime_kind: str
    runtime_version: str
    sdk_language: str | None = None
    sdk_name: str = "rbe-sdk"
    sdk_version: str = SDK_VERSION
    abi_min: int = LIBRARY_ABI_VERSION
    abi_max: int = LIBRARY_ABI_VERSION

    def validate(self) -> "LibraryWorkerIdentity":
        for name, value in (
            ("package_name", self.package_name),
            ("package_version", self.package_version),
            ("artifact_sha256", self.artifact_sha256),
            ("runtime_kind", self.runtime_kind),
            ("runtime_version", self.runtime_version),
            ("sdk_name", self.sdk_name),
            ("sdk_version", self.sdk_version),
        ):
            if not isinstance(value, str) or not value:
                raise ValueError(f"{name} must be a non-empty string")
        if self.sdk_language is not None and not self.sdk_language:
            raise ValueError("sdk_language must be a non-empty string when provided")
        if self.abi_min < 1 or self.abi_min > self.abi_max:
            raise ValueError("invalid RBE ABI range")
        return self


@dataclass(frozen=True, slots=True)
class LibraryInvocation:
    package: str
    export: str
    operation: str
    payload: Any
    rbe: RbeSdk
    session: HostSessionInfo


def _read_exact(reader: BinaryIO, size: int) -> bytes | None:
    chunks = bytearray()
    while len(chunks) < size:
        chunk = reader.read(size - len(chunks))
        if not chunk:
            if not chunks:
                return None
            raise LibraryProtocolError("RBE Library Protocol stream ended inside a frame")
        chunks.extend(chunk)
    return bytes(chunks)


def read_library_frame(reader: BinaryIO) -> dict[str, Any] | None:
    header = _read_exact(reader, 4)
    if header is None:
        return None
    (length,) = struct.unpack(">I", header)
    if length < 1 or length > MAX_LIBRARY_FRAME_BYTES:
        raise LibraryProtocolError(f"invalid RBE Library Protocol frame length {length}")
    payload = _read_exact(reader, length)
    if payload is None:
        raise LibraryProtocolError("RBE Library Protocol frame payload is missing")
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise LibraryProtocolError("invalid RBE Library Protocol JSON") from error
    if not isinstance(value, dict):
        raise LibraryProtocolError("RBE Library Protocol message must be an object")
    return value


def write_library_frame(writer: BinaryIO, value: dict[str, Any]) -> None:
    payload = json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    if len(payload) < 1 or len(payload) > MAX_LIBRARY_FRAME_BYTES:
        raise LibraryProtocolError(f"invalid RBE Library Protocol frame length {len(payload)}")
    writer.write(struct.pack(">I", len(payload)))
    writer.write(payload)
    writer.flush()


def _encode_payload(value: Any) -> list[int]:
    if value is None:
        raw = b"null"
    else:
        raw = json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    if len(raw) > MAX_LIBRARY_PAYLOAD_BYTES:
        raise LibraryProtocolError("RBE Library payload exceeds protocol limit")
    return list(raw)


def _decode_payload(value: Any) -> Any:
    if value is None:
        return None
    if not isinstance(value, list) or len(value) > MAX_LIBRARY_PAYLOAD_BYTES:
        raise LibraryProtocolError("RBE Library payload must be a bounded byte array")
    raw = bytearray()
    for index, item in enumerate(value):
        if not isinstance(item, int) or isinstance(item, bool) or not 0 <= item <= 255:
            raise LibraryProtocolError(f"payload[{index}] must be an integer byte")
        raw.append(item)
    if not raw:
        return None
    try:
        return json.loads(bytes(raw).decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise LibraryProtocolError("RBE Library payload is not valid UTF-8 JSON") from error


def _positive_int(value: Any, field: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise LibraryProtocolError(f"{field} must be a positive integer")
    return value


def _text(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise LibraryProtocolError(f"{field} must be a non-empty string")
    return value


def _parse_accept(message: dict[str, Any]) -> HostSessionInfo:
    if message.get("type") == "library.reject":
        raise LibraryProtocolError(
            f"{message.get('code', 'LIBRARY_REJECT')}: {message.get('message', 'worker rejected')}"
        )
    if message.get("type") != "library.accept":
        raise LibraryProtocolError(f"expected library.accept, got {message.get('type')!r}")
    capabilities = message.get("grantedCapabilities", [])
    features = message.get("features", [])
    if not isinstance(capabilities, list) or not all(isinstance(v, str) and v for v in capabilities):
        raise LibraryProtocolError("grantedCapabilities must be an array of non-empty strings")
    if not isinstance(features, list) or not all(isinstance(v, str) and v for v in features):
        raise LibraryProtocolError("features must be an array of non-empty strings")
    return HostSessionInfo(
        protocol=_positive_int(message.get("protocol"), "library.accept.protocol"),
        abi=_positive_int(message.get("abi"), "library.accept.abi"),
        capability_identity=_text(message.get("capabilityIdentity"), "capabilityIdentity"),
        granted_capabilities=tuple(capabilities),
        features=tuple(features),
    ).validate()


class LibraryWorkerBridge:
    def __init__(self, reader: BinaryIO, writer: BinaryIO, session: HostSessionInfo) -> None:
        self._reader = reader
        self._writer = writer
        self._session = session
        self._next_call_id = 1

    def session_info(self) -> HostSessionInfo:
        return self._session

    def call(self, request: HostCall) -> Any:
        call_id = self._next_call_id
        self._next_call_id += 1
        write_library_frame(
            self._writer,
            {
                "type": "host.call",
                "call_id": call_id,
                "capability": _text(request.capability, "capability"),
                "target": _text(request.target, "target"),
                "operation": _text(request.operation, "operation"),
                "payload": _encode_payload(request.payload),
            },
        )
        while True:
            reply = read_library_frame(self._reader)
            if reply is None:
                raise LibraryProtocolError("RBE Library Host closed while a host call was pending")
            if reply.get("type") == "library.reject":
                raise LibraryProtocolError(
                    f"{reply.get('code', 'LIBRARY_REJECT')}: {reply.get('message', 'host rejected worker')}"
                )
            if reply.get("type") != "host.reply":
                raise LibraryProtocolError(
                    f"unexpected {reply.get('type')!r} while waiting for host.reply"
                )
            if reply.get("call_id") != call_id:
                raise LibraryProtocolError("RBE Library Host reply call_id mismatch")
            if reply.get("ok") is not True:
                raise LibraryProtocolError(str(reply.get("error") or "RBE Library Host call failed"))
            return _decode_payload(reply.get("payload", []))


def _safe_error(error: BaseException) -> str:
    message = str(error) or "package invocation failed"
    message = "".join(" " if ord(char) < 32 or ord(char) == 127 else char for char in message)
    return message[:65536]


def run_library_worker(
    identity: LibraryWorkerIdentity,
    invoke: Callable[[LibraryInvocation], Any],
    *,
    reader: BinaryIO | None = None,
    writer: BinaryIO | None = None,
) -> None:
    identity = identity.validate()
    if not callable(invoke):
        raise TypeError("RBE Library worker invoke must be callable")
    reader = reader or sys.stdin.buffer
    writer = writer or sys.stdout.buffer
    sdk_language = identity.sdk_language or identity.runtime_kind

    write_library_frame(
        writer,
        {
            "type": "library.hello",
            "protocol": LIBRARY_PROTOCOL_VERSION,
            "package": {
                "name": identity.package_name,
                "version": identity.package_version,
                "artifact_sha256": identity.artifact_sha256,
            },
            "sdk": {
                "language": sdk_language,
                "name": identity.sdk_name,
                "version": identity.sdk_version,
            },
            "runtime": {"kind": identity.runtime_kind, "version": identity.runtime_version},
            "abi_min": identity.abi_min,
            "abi_max": identity.abi_max,
        },
    )
    accepted = read_library_frame(reader)
    if accepted is None:
        raise LibraryProtocolError("RBE Library Host closed before handshake acceptance")
    session = _parse_accept(accepted)
    bridge = LibraryWorkerBridge(reader, writer, session)
    rbe = RbeSdk(bridge)

    while True:
        message = read_library_frame(reader)
        if message is None:
            return
        if message.get("type") == "library.reject":
            raise LibraryProtocolError(
                f"{message.get('code', 'LIBRARY_REJECT')}: {message.get('message', 'host rejected worker')}"
            )
        if message.get("type") != "library.invoke":
            raise LibraryProtocolError(f"unexpected Library Protocol message {message.get('type')!r}")
        call_id = message.get("call_id")
        if not isinstance(call_id, int) or isinstance(call_id, bool) or call_id < 0:
            raise LibraryProtocolError("library.invoke.call_id must be a non-negative integer")
        export = _text(message.get("export"), "library.invoke.export")
        operation = _text(message.get("operation"), "library.invoke.operation")
        try:
            result = invoke(
                LibraryInvocation(
                    package=identity.package_name,
                    export=export,
                    operation=operation,
                    payload=_decode_payload(message.get("payload", [])),
                    rbe=rbe,
                    session=session,
                )
            )
            if inspect.isawaitable(result):
                result = asyncio.run(result)
            write_library_frame(
                writer,
                {
                    "type": "library.reply",
                    "call_id": call_id,
                    "ok": True,
                    "payload": _encode_payload(result),
                    "error": None,
                },
            )
        except BaseException as error:
            write_library_frame(
                writer,
                {
                    "type": "library.reply",
                    "call_id": call_id,
                    "ok": False,
                    "payload": [],
                    "error": _safe_error(error),
                },
            )
