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

NET_METHODS = (
    "http",
    "cookies",
    "headers",
    "url",
    "dns",
    "ip",
    "tcp",
    "udp",
    "quic",
    "websocket",
    "webtransport",
    "p2p",
    "mask",
)

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

for method in NET_METHODS:
    require(RUST, f"pub fn {method}(self) -> NetLibrary", f"Rust net.{method}")
    require(JS, f"{method}() {{ return new CapabilityClient", f"JS net.{method}")
    require(TS, f"{method}(): CapabilityClient;", f"TypeScript net.{method}")
    require(PY, f"def {method}(self) -> CapabilityClient:", f"Python net.{method}")

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
