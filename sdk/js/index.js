export const LIBRARY_ABI_VERSION = 1;
export const SDK_VERSION = "0.1.0";

export const capability = Object.freeze({
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

export function libraryDescriptor({ name, version, abiMin = 1, abiMax = abiMin }) {
  if (!validComponent(name)) {
    throw new TypeError(`invalid RBE library name ${JSON.stringify(name)}`);
  }
  if (!Number.isInteger(abiMin) || !Number.isInteger(abiMax) || abiMin < 1 || abiMin > abiMax) {
    throw new TypeError(`invalid RBE ABI range ${abiMin}..=${abiMax}`);
  }
  return Object.freeze({ name, version, abiMin, abiMax });
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
}

class NetClient {
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

  hostBridge() {
    return this.bridge;
  }
}

export class RbeSdk {
  constructor(bridge) {
    assertBridge(bridge);
    this.bridge = bridge;
  }

  net() { return new NetClient(this.bridge); }
  router() { return new RouterClient(this.bridge); }
  storage() { return new CapabilityClient(this.bridge, capability.STORAGE, "storage"); }
  crypto() { return new CapabilityClient(this.bridge, capability.CRYPTO, "crypto"); }

  capability(capabilityId, target = capabilityId) {
    return new CapabilityClient(this.bridge, capabilityId, target);
  }

  advanced() {
    return new AdvancedClient(this.bridge);
  }

  hostBridge() {
    return this.bridge;
  }

  call(request) {
    return this.bridge.call(request);
  }
}
