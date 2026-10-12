#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parent
RUST = (ROOT / "rbe-sdk" / "src" / "lib.rs").read_text(encoding="utf-8")
JS = (ROOT / "js" / "index.js").read_text(encoding="utf-8")
TS = (ROOT / "js" / "index.d.ts").read_text(encoding="utf-8")
PY = (ROOT / "python" / "rbe_sdk" / "__init__.py").read_text(encoding="utf-8")

CAPABILITIES = {
    "LOG": "log",
    "NET_HTTP": "net:http",
    "NET_COOKIES": "net:cookies",
    "NET_HEADERS": "net:headers",
    "NET_URL": "net:url",
    "NET_DNS": "net:dns",
    "NET_IP": "net:ip",
    "NET_TCP": "net:tcp",
    "NET_TCP_LISTEN": "net:tcp-listen",
    "NET_TLS": "net:tls",
    "NET_UDP": "net:udp",
    "NET_QUIC": "net:quic",
    "NET_WEBSOCKET": "net:websocket",
    "NET_WEBTRANSPORT": "net:webtransport",
    "NET_P2P": "net:p2p",
    "NET_MASK": "net:mask",
    "ROUTER_READ": "router:read",
    "ROUTER_REGISTER": "router:register",
    "STORAGE": "storage",
    "CRYPTO": "crypto",
}

GENERIC_NET_METHODS = (
    "http",
    "cookies",
    "headers",
    "url",
    "ip",
    "udp",
    "quic",
    "websocket",
    "webtransport",
    "p2p",
    "mask",
)

DNS_METHODS = ("lookup", "ip", "mx")
TCP_METHODS = ("connect", "write", "read", "close")
TLS_METHODS = ("connect", "write", "read", "start_tls", "close")
TCP_LISTEN_METHODS = ("bind", "accept", "write", "read", "start_tls", "close_connection", "close")
LOG_METHODS = ("debug", "info", "warn", "error", "fatal")


def require(source: str, needle: str, label: str) -> None:
    if needle not in source:
        raise SystemExit(f"SDK parity violation: {label} is missing {needle!r}")


def class_slice(source: str, start: str, end: str) -> str:
    begin = source.find(start)
    if begin < 0:
        raise SystemExit(f"SDK parity violation: missing {start!r}")
    finish = source.find(end, begin + len(start))
    if finish < 0:
        finish = len(source)
    return source[begin:finish]


for name, value in CAPABILITIES.items():
    require(RUST, f'pub const {name}: &str = "{value}";', f"Rust capability {name}")
    require(JS, f'{name}: "{value}"', f"JS capability {name}")
    require(PY, f'{name} = "{value}"', f"Python capability {name}")

for method in GENERIC_NET_METHODS:
    require(RUST, f"pub fn {method}(self) -> NetLibrary", f"Rust net.{method}")
    require(JS, f"{method}() {{ return new CapabilityClient", f"JS net.{method}")
    require(TS, f"{method}(): CapabilityClient;", f"TypeScript net.{method}")
    require(PY, f"def {method}(self) -> CapabilityClient:", f"Python net.{method}")

require(RUST, "pub fn dns(self) -> Dns", "Rust typed net.dns")
require(RUST, "pub fn tcp(self) -> Tcp", "Rust typed net.tcp")
require(RUST, "pub fn tcp_listen(self) -> TcpListen", "Rust typed net.tcp_listen")
require(RUST, "pub fn tls(self) -> Tls", "Rust typed net.tls")
require(JS, "dns() { return new DnsClient", "JS typed net.dns")
require(JS, "tcp() { return new TcpClient", "JS typed net.tcp")
require(JS, "tcpListen() { return new TcpListenerClient", "JS typed net.tcpListen")
require(JS, "tls() { return new TlsClient", "JS typed net.tls")
require(TS, "dns(): DnsClient;", "TypeScript typed net.dns")
require(TS, "tcp(): TcpClient;", "TypeScript typed net.tcp")
require(TS, "tcpListen(): TcpListenerClient;", "TypeScript typed net.tcpListen")
require(TS, "tls(): TlsClient;", "TypeScript typed net.tls")
require(PY, "def dns(self) -> DnsClient:", "Python typed net.dns")
require(PY, "def tcp(self) -> TcpClient:", "Python typed net.tcp")
require(PY, "def tcp_listen(self) -> TcpListenerClient:", "Python typed net.tcp_listen")
require(PY, "def tls(self) -> TlsClient:", "Python typed net.tls")

rust_dns = class_slice(RUST, "impl<'a> Dns<'a>", "/// Typed stateful TCP client")
js_dns = class_slice(JS, "export class DnsClient", "export class TcpClient")
py_dns = class_slice(PY, "class DnsClient", "class TcpClient")
for method in DNS_METHODS:
    require(rust_dns, f"pub fn {method}(&self", f"Rust DNS.{method}")
    require(js_dns, f"{method}(name)", f"JS DNS.{method}")
    require(py_dns, f"def {method}(self, name: str)", f"Python DNS.{method}")
    require(TS, f"{method}(name: string):", f"TypeScript DNS.{method}")

rust_tcp = class_slice(RUST, "impl<'a> Tcp<'a>", "/// Typed TLS/STARTTLS client")
js_tcp = class_slice(JS, "export class TcpClient", "export class TlsClient")
py_tcp = class_slice(PY, "class TcpClient", "class TlsClient")
for method in TCP_METHODS:
    require(rust_tcp, f"pub fn {method}(", f"Rust TCP.{method}")
    require(js_tcp, f"{method}(", f"JS TCP.{method}")
    require(py_tcp, f"def {method}(", f"Python TCP.{method}")
    require(TS, f"{method}(", f"TypeScript TCP.{method}")


rust_tls = class_slice(RUST, "impl<'a> Tls<'a>", "/// Typed inbound TCP listener client")
js_tls = class_slice(JS, "export class TlsClient", "export class TcpListenerClient")
py_tls = class_slice(PY, "class TlsClient", "class TcpListenerClient")
for method in TLS_METHODS:
    js_name = "startTls" if method == "start_tls" else method
    require(rust_tls, f"pub fn {method}(", f"Rust TLS.{method}")
    require(js_tls, f"{js_name}(", f"JS TLS.{js_name}")
    require(py_tls, f"def {method}(", f"Python TLS.{method}")
    require(TS, f"{js_name}(", f"TypeScript TLS.{js_name}")

rust_listener = class_slice(RUST, "impl<'a> TcpListen<'a>", "/// Generic reusable client")
js_listener = class_slice(JS, "export class TcpListenerClient", "export class NetClient")
py_listener = class_slice(PY, "class TcpListenerClient", "class NetClient")
for method in TCP_LISTEN_METHODS:
    js_name = {"start_tls": "startTls", "close_connection": "closeConnection"}.get(method, method)
    require(rust_listener, f"pub fn {method}(", f"Rust TCP listener.{method}")
    require(js_listener, f"{js_name}(", f"JS TCP listener.{js_name}")
    require(py_listener, f"def {method}(", f"Python TCP listener.{method}")
    require(TS, f"{js_name}(", f"TypeScript TCP listener.{js_name}")

require(RUST, "pub fn raw(&self) -> &CapabilityClient", "Rust typed net raw escape hatch")
require(JS, "extends CapabilityClient", "JS typed net raw escape hatch")
require(PY, "class DnsClient(CapabilityClient)", "Python DNS raw escape hatch")
require(PY, "class TcpClient(CapabilityClient)", "Python TCP raw escape hatch")

require(RUST, "pub fn log(self, library_name: &str)", "Rust log")
require(JS, "log(libraryName) {", "JS explicit log identity")
require(TS, "log(libraryName: string): LoggerClient;", "TypeScript explicit log identity")
require(PY, "def log(self, library_name: str) -> LoggerClient:", "Python explicit log identity")

for method in LOG_METHODS:
    require(RUST, f"pub fn {method}(&self, message:", f"Rust logger.{method}")
    require(JS, f'{method}(message) {{ return this.emit("{method}", message); }}', f"JS logger.{method}")
    require(TS, f"{method}(message: unknown):", f"TypeScript logger.{method}")
    require(PY, f"def {method}(self, message: Any) -> Any:", f"Python logger.{method}")

require(RUST, 'format!("lib/{}", self.library_name)', "Rust lib/<name> log target")
require(JS, "return `lib/${this.libraryName}`;", "JS lib/<name> log target")
require(PY, 'return f"lib/{self.library_name}"', "Python lib/<name> log target")

rust_advanced = class_slice(RUST, "impl<'a> AdvancedSdk<'a>", "/// Read-only view")
js_advanced = class_slice(JS, "export class AdvancedClient", "export class RbeSdk")
py_advanced = class_slice(PY, "class AdvancedClient", "class RbeSdk")
for source, label in ((rust_advanced, "Rust"), (js_advanced, "JS"), (py_advanced, "Python")):
    if " net(" in source or "def net(" in source or "pub fn net(" in source:
        raise SystemExit(
            f"SDK parity violation: {label} advanced surface must stay generic; typed net belongs on net()"
        )

print("SDK parity OK: Rust, JavaScript/TypeScript, and Python expose the same package surface")
