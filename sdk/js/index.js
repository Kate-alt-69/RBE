export const LIBRARY_ABI_VERSION = 1;
export const SDK_VERSION = "0.1.0";

export const capability = Object.freeze({
  LOG: "log",
  NET_HTTP: "net:http",
  NET_COOKIES: "net:cookies",
  NET_HEADERS: "net:headers",
  NET_URL: "net:url",
  NET_DNS: "net:dns",
  NET_IP: "net:ip",
  NET_TCP: "net:tcp",
  NET_UDP: "net:udp",
  NET_QUIC: "net:quic",
  NET_WEBSOCKET: "net:websocket",
  NET_WEBTRANSPORT: "net:webtransport",
  NET_P2P: "net:p2p",
  NET_MASK: "net:mask",
  ROUTER_READ: "router:read",
  ROUTER_REGISTER: "router:register",
  STORAGE: "storage",
  CRYPTO: "crypto"
});

function assertBridge(bridge) {
  if (!bridge || typeof bridge.call !== "function") {
    throw new TypeError("RBE HostBridge must provide call(request)");
  }
}

function validComponent(value) {
  return typeof value === "string" && /^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(value);
}

function assertLibraryName(value) {
  if (!validComponent(value)) {
    throw new TypeError(`invalid RBE library name ${JSON.stringify(value)}`);
  }
  return value;
}

function assertNonEmptyString(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${label} must be a non-empty string`);
  }
  return value;
}

function assertPort(value) {
  if (!Number.isInteger(value) || value < 1 || value > 65535) {
    throw new RangeError("TCP port must be an integer in 1..=65535");
  }
  return value;
}

function assertPositiveInteger(value, label) {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new RangeError(`${label} must be a positive safe integer`);
  }
  return value;
}

function optionalTimeout(payload, timeoutMs) {
  if (timeoutMs !== null && timeoutMs !== undefined) {
    payload.timeout_ms = assertPositiveInteger(timeoutMs, "timeoutMs");
  }
  return payload;
}

function byteArray(data) {
  if (data instanceof Uint8Array) return Array.from(data);
  if (Array.isArray(data) && data.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
    return [...data];
  }
  throw new TypeError("TCP write data must be Uint8Array or an array of byte values");
}

function isPromiseLike(value) {
  return value && typeof value.then === "function";
}

function syncHookResult(name, value) {
  if (isPromiseLike(value)) {
    throw new TypeError(`RBE SDK interceptor ${name} hook must be synchronous`);
  }
  return value;
}

function stringList(name, value) {
  if (value === undefined) return Object.freeze([]);
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string" || item.length === 0)) {
    throw new TypeError(`RBE HostBridge sessionInfo().${name} must be an array of non-empty strings`);
  }
  return Object.freeze([...new Set(value)]);
}

function normalizeSessionInfo(value) {
  if (value == null) return null;
  if (typeof value !== "object") {
    throw new TypeError("RBE HostBridge sessionInfo() must return an object or null");
  }
  if (!Number.isInteger(value.protocol) || value.protocol < 1) {
    throw new TypeError("RBE HostBridge sessionInfo().protocol must be a positive integer");
  }
  if (!Number.isInteger(value.abi) || value.abi < 1) {
    throw new TypeError("RBE HostBridge sessionInfo().abi must be a positive integer");
  }
  if (typeof value.capabilityIdentity !== "string" || value.capabilityIdentity.length === 0) {
    throw new TypeError("RBE HostBridge sessionInfo().capabilityIdentity must be a non-empty string");
  }
  return Object.freeze({
    protocol: value.protocol,
    abi: value.abi,
    capabilityIdentity: value.capabilityIdentity,
    grantedCapabilities: stringList("grantedCapabilities", value.grantedCapabilities),
    features: stringList("features", value.features)
  });
}

function bridgeSessionInfo(bridge) {
  const sessionInfo = bridge?.sessionInfo;
  if (typeof sessionInfo !== "function") return null;
  const value = sessionInfo.call(bridge);
  if (isPromiseLike(value)) {
    throw new TypeError("RBE HostBridge sessionInfo() must be synchronous handshake metadata");
  }
  return normalizeSessionInfo(value);
}

export function libraryDescriptor({ name, version, abiMin = 1, abiMax = abiMin }) {
  assertLibraryName(name);
  if (!Number.isInteger(abiMin) || !Number.isInteger(abiMax) || abiMin < 1 || abiMin > abiMax) {
    throw new TypeError(`invalid RBE ABI range ${abiMin}..=${abiMax}`);
  }
  return Object.freeze({ name, version, abiMin, abiMax });
}

/**
 * SDK-local HostBridge wrapper for package-defined instrumentation and call policy.
 * Hooks never grant capabilities; the wrapped RBE HostBridge remains authoritative.
 */
export class InterceptedBridge {
  constructor(bridge, interceptors = []) {
    assertBridge(bridge);
    if (!Array.isArray(interceptors)) {
      throw new TypeError("RBE SDK interceptors must be an array");
    }
    this.bridge = bridge;
    this.interceptors = Object.freeze([...interceptors]);
  }

  sessionInfo() {
    return bridgeSessionInfo(this.bridge);
  }

  call(request) {
    let current = { ...request };

    for (const interceptor of this.interceptors) {
      if (!interceptor || typeof interceptor !== "object") {
        throw new TypeError("RBE SDK interceptor must be an object");
      }
      if (typeof interceptor.before === "function") {
        const next = syncHookResult("before", interceptor.before(current));
        if (next !== undefined) current = next;
      }
    }

    const after = (reply) => {
      let currentReply = reply;
      for (let index = this.interceptors.length - 1; index >= 0; index -= 1) {
        const interceptor = this.interceptors[index];
        if (typeof interceptor.after === "function") {
          const next = syncHookResult("after", interceptor.after(current, currentReply));
          if (next !== undefined) currentReply = next;
        }
      }
      return currentReply;
    };

    const onError = (error) => {
      let currentError = error;
      for (let index = this.interceptors.length - 1; index >= 0; index -= 1) {
        const interceptor = this.interceptors[index];
        if (typeof interceptor.onError === "function") {
          const next = syncHookResult("onError", interceptor.onError(current, currentError));
          if (next !== undefined) currentError = next;
        }
      }
      throw currentError;
    };

    try {
      const result = this.bridge.call(current);
      if (isPromiseLike(result)) {
        return Promise.resolve(result).then(after, onError);
      }
      return after(result);
    } catch (error) {
      return onError(error);
    }
  }
}

export class HostClient {
  constructor(bridge) {
    assertBridge(bridge);
    this.bridge = bridge;
  }

  session() {
    return bridgeSessionInfo(this.bridge);
  }

  selectedAbi() {
    return this.session()?.abi ?? null;
  }

  capabilityIdentity() {
    return this.session()?.capabilityIdentity ?? null;
  }

  granted(capabilityId) {
    const session = this.session();
    return session ? session.grantedCapabilities.includes(capabilityId) : null;
  }

  supports(feature) {
    const session = this.session();
    return session ? session.features.includes(feature) : null;
  }
}

export class CapabilityClient {
  constructor(bridge, capabilityId, target = capabilityId) {
    assertBridge(bridge);
    this.bridge = bridge;
    this.capabilityId = capabilityId;
    this.target = target;
  }

  request(operation, payload = null) {
    return {
      capability: this.capabilityId,
      target: this.target,
      operation,
      payload
    };
  }

  call(operation, payload = null) {
    return this.bridge.call(this.request(operation, payload));
  }

  retarget(target) {
    return new CapabilityClient(this.bridge, this.capabilityId, target);
  }

  intercept(...interceptors) {
    return new CapabilityClient(
      new InterceptedBridge(this.bridge, interceptors),
      this.capabilityId,
      this.target
    );
  }
}

export class LoggerClient {
  constructor(bridge, libraryName, scope = []) {
    assertBridge(bridge);
    this.bridge = bridge;
    this.libraryName = assertLibraryName(libraryName);
    this.scope = Object.freeze([...scope]);
  }

  target() {
    return `lib/${this.libraryName}`;
  }

  child(name) {
    if (!validComponent(name)) {
      throw new TypeError(`invalid RBE logger child scope ${JSON.stringify(name)}`);
    }
    return new LoggerClient(this.bridge, this.libraryName, [...this.scope, name]);
  }

  emit(level, message) {
    return new CapabilityClient(this.bridge, capability.LOG, this.target()).call(level, {
      scope: this.scope,
      message: String(message)
    });
  }

  debug(message) { return this.emit("debug", message); }
  info(message) { return this.emit("info", message); }
  warn(message) { return this.emit("warn", message); }
  error(message) { return this.emit("error", message); }
  fatal(message) { return this.emit("fatal", message); }
}

export class DnsClient extends CapabilityClient {
  constructor(bridge) {
    super(bridge, capability.NET_DNS);
  }

  lookup(name) { return this.call("lookup", { name: assertNonEmptyString(name, "DNS name") }); }
  ip(name) { return this.call("ip", { name: assertNonEmptyString(name, "DNS name") }); }
  mx(name) { return this.call("mx", { name: assertNonEmptyString(name, "DNS name") }); }
}

export class TcpClient extends CapabilityClient {
  constructor(bridge) {
    super(bridge, capability.NET_TCP);
  }

  connect(host, port, timeoutMs = null) {
    return this.call("connect", optionalTimeout({
      host: assertNonEmptyString(host, "TCP host"),
      port: assertPort(port)
    }, timeoutMs));
  }

  write(handle, data, timeoutMs = null) {
    return this.call("write", optionalTimeout({
      handle: assertNonEmptyString(handle, "TCP handle"),
      data: byteArray(data)
    }, timeoutMs));
  }

  read(handle, maxBytes, timeoutMs = null) {
    return this.call("read", optionalTimeout({
      handle: assertNonEmptyString(handle, "TCP handle"),
      max_bytes: assertPositiveInteger(maxBytes, "maxBytes")
    }, timeoutMs));
  }

  close(handle) {
    return this.call("close", { handle: assertNonEmptyString(handle, "TCP handle") });
  }
}

export class NetClient {
  constructor(bridge) {
    this.bridge = bridge;
  }

  sublibrary(name) {
    if (!validComponent(name)) {
      throw new TypeError(`invalid net sub-library name ${JSON.stringify(name)}`);
    }
    return new CapabilityClient(this.bridge, `net:${name}`);
  }

  http() { return new CapabilityClient(this.bridge, capability.NET_HTTP); }
  cookies() { return new CapabilityClient(this.bridge, capability.NET_COOKIES); }
  headers() { return new CapabilityClient(this.bridge, capability.NET_HEADERS); }
  url() { return new CapabilityClient(this.bridge, capability.NET_URL); }
  dns() { return new DnsClient(this.bridge); }
  ip() { return new CapabilityClient(this.bridge, capability.NET_IP); }
  tcp() { return new TcpClient(this.bridge); }
  udp() { return new CapabilityClient(this.bridge, capability.NET_UDP); }
  quic() { return new CapabilityClient(this.bridge, capability.NET_QUIC); }
  websocket() { return new CapabilityClient(this.bridge, capability.NET_WEBSOCKET); }
  webtransport() { return new CapabilityClient(this.bridge, capability.NET_WEBTRANSPORT); }
  p2p() { return new CapabilityClient(this.bridge, capability.NET_P2P); }
  mask() { return new CapabilityClient(this.bridge, capability.NET_MASK); }
}

class RouterClient {
  constructor(bridge) {
    this.bridge = bridge;
  }

  inspect(operation, payload = null) {
    return new CapabilityClient(this.bridge, capability.ROUTER_READ, "router").call(operation, payload);
  }

  register(operation, payload = null) {
    return new CapabilityClient(this.bridge, capability.ROUTER_REGISTER, "router").call(operation, payload);
  }
}

export class AdvancedClient {
  constructor(bridge) {
    assertBridge(bridge);
    this.bridge = bridge;
  }

  capability(capabilityId, target = capabilityId) {
    return new CapabilityClient(this.bridge, capabilityId, target);
  }

  request(capabilityId, target, operation, payload = null) {
    return { capability: capabilityId, target, operation, payload };
  }

  send(request) {
    return this.bridge.call(request);
  }

  async batch(requests) {
    return Promise.all(requests.map(async (request) => {
      try {
        return { ok: true, value: await this.send(request) };
      } catch (error) {
        return { ok: false, error };
      }
    }));
  }

  host() {
    return new HostClient(this.bridge);
  }

  intercept(...interceptors) {
    return new AdvancedClient(new InterceptedBridge(this.bridge, interceptors));
  }

  hostBridge() {
    return this.bridge;
  }
}

export class RbeSdk {
  constructor(bridge, libraryName = null) {
    assertBridge(bridge);
    if (libraryName !== null) assertLibraryName(libraryName);
    this.bridge = bridge;
    this.libraryName = libraryName;
  }

  log(libraryName) {
    return new LoggerClient(this.bridge, assertLibraryName(libraryName));
  }

  net() { return new NetClient(this.bridge); }
  router() { return new RouterClient(this.bridge); }
  storage() { return new CapabilityClient(this.bridge, capability.STORAGE, "storage"); }
  crypto() { return new CapabilityClient(this.bridge, capability.CRYPTO, "crypto"); }
  host() { return new HostClient(this.bridge); }

  capability(capabilityId, target = capabilityId) {
    return new CapabilityClient(this.bridge, capabilityId, target);
  }

  advanced() {
    return new AdvancedClient(this.bridge);
  }

  intercept(...interceptors) {
    return new RbeSdk(new InterceptedBridge(this.bridge, interceptors), this.libraryName);
  }

  hostBridge() {
    return this.bridge;
  }

  call(request) {
    return this.bridge.call(request);
  }
}
