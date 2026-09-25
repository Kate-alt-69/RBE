"""Stable Python SDK contract for external RBE libraries."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol

LIBRARY_ABI_VERSION = 1
SDK_VERSION = "0.1.0"

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


@dataclass(frozen=True, slots=True)
class LibraryDescriptor:
    name: str
    version: str
    abi_min: int = LIBRARY_ABI_VERSION
    abi_max: int = LIBRARY_ABI_VERSION

    def validate(self) -> "LibraryDescriptor":
        if not _valid_component(self.name):
            raise ValueError(f"invalid RBE library name {self.name!r}")
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


class HostBridge(Protocol):
    def call(self, request: HostCall) -> Any: ...


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


class NetClient:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    def sublibrary(self, name: str) -> CapabilityClient:
        if not _valid_component(name):
            raise ValueError(f"invalid net sub-library name {name!r}")
        return CapabilityClient(self._bridge, f"net:{name}")

    def http(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, NET_HTTP)

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

    def intercept(self, *interceptors: HostInterceptor) -> "AdvancedClient":
        return AdvancedClient(InterceptedBridge(self._bridge, tuple(interceptors)))

    def host_bridge(self) -> HostBridge:
        return self._bridge


class RbeSdk:
    def __init__(self, bridge: HostBridge) -> None:
        if not callable(getattr(bridge, "call", None)):
            raise TypeError("RBE HostBridge must provide call(request)")
        self._bridge = bridge

    def net(self) -> NetClient:
        return NetClient(self._bridge)

    def router(self) -> RouterClient:
        return RouterClient(self._bridge)

    def storage(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, STORAGE, "storage")

    def crypto(self) -> CapabilityClient:
        return CapabilityClient(self._bridge, CRYPTO, "crypto")

    def capability(self, capability: str, target: str | None = None) -> CapabilityClient:
        return CapabilityClient(self._bridge, capability, target)

    def advanced(self) -> AdvancedClient:
        return AdvancedClient(self._bridge)

    def intercept(self, *interceptors: HostInterceptor) -> "RbeSdk":
        return RbeSdk(InterceptedBridge(self._bridge, tuple(interceptors)))

    def host_bridge(self) -> HostBridge:
        return self._bridge

    def call(self, request: HostCall) -> Any:
        return self._bridge.call(request)
