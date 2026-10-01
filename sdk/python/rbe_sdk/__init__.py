"""Stable Python SDK contract for external RBE libraries."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol

LIBRARY_ABI_VERSION = 1
SDK_VERSION = "0.1.0"

LOG = "log"
NET_HTTP = "net:http"
NET_COOKIES = "net:cookies"
NET_HEADERS = "net:headers"
NET_URL = "net:url"
NET_DNS = "net:dns"
NET_IP = "net:ip"
NET_TCP = "net:tcp"
NET_UDP = "net:udp"
NET_QUIC = "net:quic"
NET_WEBSOCKET = "net:websocket"
NET_WEBTRANSPORT = "net:webtransport"
NET_P2P = "net:p2p"
NET_MASK = "net:mask"
ROUTER_READ = "router:read"
ROUTER_REGISTER = "router:register"
STORAGE = "storage"
CRYPTO = "crypto"


def _valid_component(value: str) -> bool:
    if not value or not value[0].isalnum():
        return False
    return all(char.isalnum() or char in "-_" for char in value)


def _assert_library_name(value: str) -> str:
    if not _valid_component(value):
        raise ValueError(f"invalid RBE library name {value!r}")
    return value


def _assert_non_empty_string(value: str, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{label} must be a non-empty string")
    return value


def _assert_positive_int(value: int, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ValueError(f"{label} must be a positive integer")
    return value


def _optional_timeout(payload: dict[str, Any], timeout_ms: int | None) -> dict[str, Any]:
    if timeout_ms is not None:
        payload["timeout_ms"] = _assert_positive_int(timeout_ms, "timeout_ms")
    return payload


def _byte_list(data: bytes | bytearray | memoryview | list[int] | tuple[int, ...]) -> list[int]:
    if isinstance(data, (bytes, bytearray, memoryview)):
        return list(bytes(data))
    if isinstance(data, (list, tuple)) and all(
        isinstance(byte, int) and not isinstance(byte, bool) and 0 <= byte <= 255
        for byte in data
    ):
        return list(data)
    raise TypeError("TCP write data must be bytes-like or a sequence of byte values")


@dataclass(frozen=True, slots=True)
class LibraryDescriptor:
    name: str
    version: str
    abi_min: int = LIBRARY_ABI_VERSION
    abi_max: int = LIBRARY_ABI_VERSION

    def validate(self) -> "LibraryDescriptor":
        _assert_library_name(self.name)
        if self.abi_min < 1 or self.abi_min > self.abi_max:
            raise ValueError(f"invalid RBE ABI range {self.abi_min}..={self.abi_max}")
        return self

    def supports_host_abi(self, host_abi: int) -> bool:
        return self.abi_min <= host_abi <= self.abi_max


@dataclass(frozen=True, slots=True)
class HostCall:
    capability: str
    target: str
    operation: str
    payload: Any = None


@dataclass(frozen=True, slots=True)
class BatchResult:
    ok: bool
    value: Any = None
    error: BaseException | None = None


@dataclass(frozen=True, slots=True)
class HostSessionInfo:
    protocol: int
    abi: int
    capability_identity: str
    granted_capabilities: tuple[str, ...] = ()
    features: tuple[str, ...] = ()

    def validate(self) -> "HostSessionInfo":
        if self.protocol < 1 or self.abi < 1:
            raise ValueError("RBE host session protocol and ABI must be positive integers")
        if not self.capability_identity:
            raise ValueError("RBE host session capability identity must be non-empty")
        if any(not value for value in self.granted_capabilities):
            raise ValueError("RBE host session capability IDs must be non-empty")
        if any(not value for value in self.features):
            raise ValueError("RBE host session feature IDs must be non-empty")
        return self

    def granted(self, capability: str) -> bool:
        return capability in self.granted_capabilities

    def supports(self, feature: str) -> bool:
        return feature in self.features


class HostBridge(Protocol):
    def call(self, request: HostCall) -> Any: ...


def _session_info(bridge: HostBridge) -> HostSessionInfo | None:
    provider = getattr(bridge, "session_info", None)
    if not callable(provider):
        return None
    value = provider()
    if value is None:
        return None
    if not isinstance(value, HostSessionInfo):
        raise TypeError("RBE HostBridge session_info() must return HostSessionInfo or None")
    return value.validate()


class HostInterceptor:
    """SDK-local hook surface around HostBridge calls.

    Hooks do not grant capabilities and do not bypass the wrapped RBE host.
    Subclasses may rewrite a request, reply, or error by returning a replacement.
    """

    def before(self, request: HostCall) -> HostCall:
        return request

    def after(self, request: HostCall, reply: Any) -> Any:
        return reply

    def on_error(self, request: HostCall, error: BaseException) -> BaseException:
        return error


class InterceptedBridge:
    def __init__(self, bridge: HostBridge, interceptors: tuple[HostInterceptor, ...]) -> None:
        if not callable(getattr(bridge, "call", None)):
            raise TypeError("RBE HostBridge must provide call(request)")
        self._bridge = bridge
        self._interceptors = interceptors

    @property
    def bridge(self) -> HostBridge:
        return self._bridge

    @property
    def interceptors(self) -> tuple[HostInterceptor, ...]:
        return self._interceptors

    def session_info(self) -> HostSessionInfo | None:
        return _session_info(self._bridge)

    def call(self, request: HostCall) -> Any:
        current = request
        for interceptor in self._interceptors:
            hook = getattr(interceptor, "before", None)
            if callable(hook):
                next_request = hook(current)
                if next_request is not None:
                    if not isinstance(next_request, HostCall):
                        raise TypeError("RBE SDK interceptor before() must return HostCall or None")
                    current = next_request

        try:
            reply = self._bridge.call(current)
        except BaseException as original_error:
            error: BaseException = original_error
            for interceptor in reversed(self._interceptors):
                hook = getattr(interceptor, "on_error", None)
                if callable(hook):
                    next_error = hook(current, error)
                    if next_error is not None:
                        if not isinstance(next_error, BaseException):
                            raise TypeError(
                                "RBE SDK interceptor on_error() must return BaseException or None"
                            )
                        error = next_error
            raise error

        current_reply = reply
        for interceptor in reversed(self._interceptors):
            hook = getattr(interceptor, "after", None)
            if callable(hook):
                current_reply = hook(current, current_reply)
        return current_reply


class HostClient:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    def session(self) -> HostSessionInfo | None:
        return _session_info(self._bridge)

    def selected_abi(self) -> int | None:
        session = self.session()
        return session.abi if session else None

    def capability_identity(self) -> str | None:
        session = self.session()
        return session.capability_identity if session else None

    def granted(self, capability: str) -> bool | None:
        session = self.session()
        return session.granted(capability) if session else None

    def supports(self, feature: str) -> bool | None:
        session = self.session()
        return session.supports(feature) if session else None


class CapabilityClient:
    def __init__(self, bridge: HostBridge, capability: str, target: str | None = None) -> None:
        self._bridge = bridge
        self.capability = capability
        self.target = target or capability

    def request(self, operation: str, payload: Any = None) -> HostCall:
        return HostCall(self.capability, self.target, operation, payload)

    def call(self, operation: str, payload: Any = None) -> Any:
        return self._bridge.call(self.request(operation, payload))

    def retarget(self, target: str) -> "CapabilityClient":
        return CapabilityClient(self._bridge, self.capability, target)

    def intercept(self, *interceptors: HostInterceptor) -> "CapabilityClient":
        return CapabilityClient(
            InterceptedBridge(self._bridge, tuple(interceptors)),
            self.capability,
            self.target,
        )


class LoggerClient:
    def __init__(
        self,
        bridge: HostBridge,
        library_name: str,
        scope: tuple[str, ...] = (),
    ) -> None:
        self._bridge = bridge
        self.library_name = _assert_library_name(library_name)
        self.scope = scope

    @property
    def target(self) -> str:
        return f"lib/{self.library_name}"

    def child(self, name: str) -> "LoggerClient":
        if not _valid_component(name):
            raise ValueError(f"invalid RBE logger child scope {name!r}")
        return LoggerClient(self._bridge, self.library_name, (*self.scope, name))

    def emit(self, level: str, message: Any) -> Any:
        if level not in {"debug", "info", "warn", "error", "fatal"}:
            raise ValueError(f"invalid RBE log level {level!r}")
        return CapabilityClient(self._bridge, LOG, self.target).call(
            level,
            {"scope": list(self.scope), "message": str(message)},
        )

    def debug(self, message: Any) -> Any:
        return self.emit("debug", message)

    def info(self, message: Any) -> Any:
        return self.emit("info", message)

    def warn(self, message: Any) -> Any:
        return self.emit("warn", message)

    def error(self, message: Any) -> Any:
        return self.emit("error", message)

    def fatal(self, message: Any) -> Any:
        return self.emit("fatal", message)


class DnsClient(CapabilityClient):
    def __init__(self, bridge: HostBridge) -> None:
        super().__init__(bridge, NET_DNS)

    def lookup(self, name: str) -> Any:
        return self.call("lookup", {"name": _assert_non_empty_string(name, "DNS name")})

    def ip(self, name: str) -> Any:
        return self.call("ip", {"name": _assert_non_empty_string(name, "DNS name")})

    def mx(self, name: str) -> Any:
        return self.call("mx", {"name": _assert_non_empty_string(name, "DNS name")})


class TcpClient(CapabilityClient):
    def __init__(self, bridge: HostBridge) -> None:
        super().__init__(bridge, NET_TCP)

    def connect(self, host: str, port: int, timeout_ms: int | None = None) -> Any:
        if port < 1 or port > 65535:
            raise ValueError("TCP port must be in 1..=65535")
        return self.call(
            "connect",
            _optional_timeout(
                {"host": _assert_non_empty_string(host, "TCP host"), "port": port},
                timeout_ms,
            ),
        )

    def write(
        self,
        handle: str,
        data: bytes | bytearray | memoryview | list[int] | tuple[int, ...],
        timeout_ms: int | None = None,
    ) -> Any:
        return self.call(
            "write",
            _optional_timeout(
                {
                    "handle": _assert_non_empty_string(handle, "TCP handle"),
                    "data": _byte_list(data),
                },
                timeout_ms,
            ),
        )

    def read(self, handle: str, max_bytes: int, timeout_ms: int | None = None) -> Any:
        return self.call(
            "read",
            _optional_timeout(
                {
                    "handle": _assert_non_empty_string(handle, "TCP handle"),
                    "max_bytes": _assert_positive_int(max_bytes, "max_bytes"),
                },
                timeout_ms,
            ),
        )

    def close(self, handle: str) -> Any:
        return self.call("close", {"handle": _assert_non_empty_string(handle, "TCP handle")})


class NetClient:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    def sublibrary(self, name: str) -> CapabilityClient:
        if not _valid_component(name):
            raise ValueError(f"invalid net sub-library name {name!r}")
        return CapabilityClient(self._bridge, f"net:{name}")

    def http(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_HTTP)

    def cookies(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_COOKIES)

    def headers(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_HEADERS)

    def url(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_URL)

    def dns(self) -> DnsClient:
        return DnsClient(self._bridge)

    def ip(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_IP)

    def tcp(self) -> TcpClient:
        return TcpClient(self._bridge)

    def udp(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_UDP)

    def quic(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_QUIC)

    def websocket(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_WEBSOCKET)

    def webtransport(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_WEBTRANSPORT)

    def p2p(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_P2P)

    def mask(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_MASK)


class RouterClient:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    def inspect(self, operation: str, payload: Any = None) -> Any:
        return CapabilityClient(self._bridge, ROUTER_READ, "router").call(operation, payload)

    def register(self, operation: str, payload: Any = None) -> Any:
        return CapabilityClient(self._bridge, ROUTER_REGISTER, "router").call(operation, payload)


class AdvancedClient:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    def capability(self, capability: str, target: str | None = None) -> CapabilityClient:
        return CapabilityClient(self._bridge, capability, target)

    def request(
        self,
        capability: str,
        target: str,
        operation: str,
        payload: Any = None,
    ) -> HostCall:
        return HostCall(capability, target, operation, payload)

    def send(self, request: HostCall) -> Any:
        return self._bridge.call(request)

    def batch(self, requests: list[HostCall] | tuple[HostCall, ...]) -> list[BatchResult]:
        results: list[BatchResult] = []
        for request in requests:
            try:
                results.append(BatchResult(ok=True, value=self.send(request)))
            except BaseException as error:
                results.append(BatchResult(ok=False, error=error))
        return results

    def host(self) -> HostClient:
        return HostClient(self._bridge)

    def intercept(self, *interceptors: HostInterceptor) -> "AdvancedClient":
        return AdvancedClient(InterceptedBridge(self._bridge, tuple(interceptors)))

    def host_bridge(self) -> HostBridge:
        return self._bridge


class RbeSdk:
    def __init__(self, bridge: HostBridge, library_name: str | None = None) -> None:
        if not callable(getattr(bridge, "call", None)):
            raise TypeError("RBE HostBridge must provide call(request)")
        if library_name is not None:
            _assert_library_name(library_name)
        self._bridge = bridge
        self._library_name = library_name

    def log(self, library_name: str) -> LoggerClient:
        return LoggerClient(self._bridge, _assert_library_name(library_name))

    def net(self) -> NetClient:
        return NetClient(self._bridge)

    def router(self) -> RouterClient:
        return RouterClient(self._bridge)

    def storage(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, STORAGE, "storage")

    def crypto(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, CRYPTO, "crypto")

    def host(self) -> HostClient:
        return HostClient(self._bridge)

    def capability(self, capability: str, target: str | None = None) -> CapabilityClient:
        return CapabilityClient(self._bridge, capability, target)

    def advanced(self) -> AdvancedClient:
        return AdvancedClient(self._bridge)

    def intercept(self, *interceptors: HostInterceptor) -> "RbeSdk":
        return RbeSdk(
            InterceptedBridge(self._bridge, tuple(interceptors)),
            self._library_name,
        )

    def host_bridge(self) -> HostBridge:
        return self._bridge

    def call(self, request: HostCall) -> Any:
        return self._bridge.call(request)
