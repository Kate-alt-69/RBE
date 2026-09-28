import fs from "node:fs";

import { LIBRARY_ABI_VERSION, RbeSdk, SDK_VERSION } from "./index.js";

export const LIBRARY_PROTOCOL_VERSION = 1;
export const MAX_LIBRARY_FRAME_BYTES = 8 * 1024 * 1024;
export const MAX_LIBRARY_PAYLOAD_BYTES = 2 * 1024 * 1024;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

function assertObject(value, name) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${name} must be an object`);
  }
  return value;
}

function assertString(value, name) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${name} must be a non-empty string`);
  }
  return value;
}

function assertPositiveInteger(value, name) {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new TypeError(`${name} must be a positive safe integer`);
  }
  return value;
}

function assertByteArray(value, name = "payload") {
  if (!Array.isArray(value) || value.length > MAX_LIBRARY_PAYLOAD_BYTES) {
    throw new TypeError(`${name} must be a bounded byte array`);
  }
  const bytes = new Uint8Array(value.length);
  for (let index = 0; index < value.length; index += 1) {
    const byte = value[index];
    if (!Number.isInteger(byte) || byte < 0 || byte > 255) {
      throw new TypeError(`${name}[${index}] must be an integer byte`);
    }
    bytes[index] = byte;
  }
  return bytes;
}

function encodePayload(value) {
  if (value === undefined) return new Uint8Array();
  const encoded = textEncoder.encode(JSON.stringify(value));
  if (encoded.byteLength > MAX_LIBRARY_PAYLOAD_BYTES) {
    throw new RangeError(`RBE Library payload exceeds ${MAX_LIBRARY_PAYLOAD_BYTES} bytes`);
  }
  return encoded;
}

function decodePayload(value) {
  const bytes = assertByteArray(value);
  if (bytes.byteLength === 0) return null;
  return JSON.parse(textDecoder.decode(bytes));
}

function readExact(fd, size) {
  const buffer = Buffer.allocUnsafe(size);
  let offset = 0;
  while (offset < size) {
    const read = fs.readSync(fd, buffer, offset, size - offset, null);
    if (read === 0) {
      if (offset === 0) return null;
      throw new Error("RBE Library Protocol stream ended inside a frame");
    }
    offset += read;
  }
  return buffer;
}

export function readLibraryFrame(fd = 0) {
  const header = readExact(fd, 4);
  if (header === null) return null;
  const length = header.readUInt32BE(0);
  if (length === 0 || length > MAX_LIBRARY_FRAME_BYTES) {
    throw new RangeError(`invalid RBE Library Protocol frame length ${length}`);
  }
  const payload = readExact(fd, length);
  if (payload === null) throw new Error("RBE Library Protocol frame payload is missing");
  const value = JSON.parse(textDecoder.decode(payload));
  return assertObject(value, "RBE Library Protocol message");
}

export function writeLibraryFrame(value, fd = 1) {
  const object = assertObject(value, "RBE Library Protocol message");
  const payload = textEncoder.encode(JSON.stringify(object));
  if (payload.byteLength === 0 || payload.byteLength > MAX_LIBRARY_FRAME_BYTES) {
    throw new RangeError(`invalid RBE Library Protocol frame length ${payload.byteLength}`);
  }
  const frame = Buffer.allocUnsafe(4 + payload.byteLength);
  frame.writeUInt32BE(payload.byteLength, 0);
  Buffer.from(payload).copy(frame, 4);
  let offset = 0;
  while (offset < frame.length) {
    offset += fs.writeSync(fd, frame, offset, frame.length - offset);
  }
}

function normalizeIdentity(input) {
  const identity = assertObject(input, "RBE Library worker identity");
  const pkg = assertObject(identity.package, "package identity");
  const runtime = assertObject(identity.runtime, "runtime identity");
  const sdk = identity.sdk == null ? {} : assertObject(identity.sdk, "SDK identity");
  const abiMin = identity.abiMin ?? LIBRARY_ABI_VERSION;
  const abiMax = identity.abiMax ?? abiMin;
  assertPositiveInteger(abiMin, "abiMin");
  assertPositiveInteger(abiMax, "abiMax");
  if (abiMin > abiMax) throw new RangeError("abiMin must not exceed abiMax");

  return Object.freeze({
    package: Object.freeze({
      name: assertString(pkg.name, "package.name"),
      version: assertString(pkg.version, "package.version"),
      artifactSha256: assertString(pkg.artifactSha256, "package.artifactSha256")
    }),
    sdk: Object.freeze({
      language: assertString(sdk.language ?? runtime.kind, "sdk.language"),
      name: assertString(sdk.name ?? "@rbe/sdk", "sdk.name"),
      version: assertString(sdk.version ?? SDK_VERSION, "sdk.version")
    }),
    runtime: Object.freeze({
      kind: assertString(runtime.kind, "runtime.kind"),
      version: assertString(runtime.version, "runtime.version")
    }),
    abiMin,
    abiMax
  });
}

function parseAccept(message) {
  if (message.type === "library.reject") {
    throw new Error(`RBE Library worker rejected: ${message.code ?? "UNKNOWN"}: ${message.message ?? "rejected"}`);
  }
  if (message.type !== "library.accept") {
    throw new Error(`expected library.accept, got ${JSON.stringify(message.type)}`);
  }
  const capabilities = Array.isArray(message.grantedCapabilities) ? message.grantedCapabilities : [];
  const features = Array.isArray(message.features) ? message.features : [];
  return Object.freeze({
    protocol: assertPositiveInteger(message.protocol, "library.accept.protocol"),
    abi: assertPositiveInteger(message.abi, "library.accept.abi"),
    capabilityIdentity: assertString(message.capabilityIdentity, "library.accept.capabilityIdentity"),
    grantedCapabilities: Object.freeze(capabilities.map((value, index) => assertString(value, `grantedCapabilities[${index}]`))),
    features: Object.freeze(features.map((value, index) => assertString(value, `features[${index}]`)))
  });
}

export class LibraryWorkerBridge {
  constructor({ inputFd = 0, outputFd = 1, session }) {
    this.inputFd = inputFd;
    this.outputFd = outputFd;
    this.session = session;
    this.nextCallId = 1;
  }

  sessionInfo() {
    return this.session;
  }

  call(request) {
    const current = assertObject(request, "RBE HostBridge request");
    const callId = this.nextCallId;
    this.nextCallId += 1;
    writeLibraryFrame({
      type: "host.call",
      call_id: callId,
      capability: assertString(current.capability, "capability"),
      target: assertString(current.target, "target"),
      operation: assertString(current.operation, "operation"),
      payload: Array.from(encodePayload(current.payload))
    }, this.outputFd);

    for (;;) {
      const reply = readLibraryFrame(this.inputFd);
      if (reply === null) throw new Error("RBE Library Host closed while a host call was pending");
      if (reply.type === "library.reject") {
        throw new Error(`${reply.code ?? "LIBRARY_REJECT"}: ${reply.message ?? "host rejected worker"}`);
      }
      if (reply.type !== "host.reply") {
        throw new Error(`unexpected ${JSON.stringify(reply.type)} while waiting for host.reply`);
      }
      if (reply.call_id !== callId) {
        throw new Error(`RBE Library Host reply call_id mismatch: expected ${callId}, got ${reply.call_id}`);
      }
      if (reply.ok !== true) {
        throw new Error(typeof reply.error === "string" ? reply.error : "RBE Library Host call failed");
      }
      return decodePayload(reply.payload ?? []);
    }
  }
}

function helloMessage(identity) {
  return {
    type: "library.hello",
    protocol: LIBRARY_PROTOCOL_VERSION,
    package: {
      name: identity.package.name,
      version: identity.package.version,
      artifact_sha256: identity.package.artifactSha256
    },
    sdk: {
      language: identity.sdk.language,
      name: identity.sdk.name,
      version: identity.sdk.version
    },
    runtime: identity.runtime,
    abi_min: identity.abiMin,
    abi_max: identity.abiMax
  };
}

function safeError(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message.replace(/[\u0000-\u001F\u007F]/g, " ").slice(0, 65536) || "package invocation failed";
}

export async function runLibraryWorker({ identity: rawIdentity, invoke, inputFd = 0, outputFd = 1 }) {
  if (typeof invoke !== "function") throw new TypeError("RBE Library worker invoke must be a function");
  const identity = normalizeIdentity(rawIdentity);
  writeLibraryFrame(helloMessage(identity), outputFd);
  const accepted = readLibraryFrame(inputFd);
  if (accepted === null) throw new Error("RBE Library Host closed before handshake acceptance");
  const session = parseAccept(accepted);
  const bridge = new LibraryWorkerBridge({ inputFd, outputFd, session });
  const rbe = new RbeSdk(bridge);

  for (;;) {
    const message = readLibraryFrame(inputFd);
    if (message === null) return;
    if (message.type === "library.reject") {
      throw new Error(`${message.code ?? "LIBRARY_REJECT"}: ${message.message ?? "host rejected worker"}`);
    }
    if (message.type !== "library.invoke") {
      throw new Error(`unexpected RBE Library Protocol message ${JSON.stringify(message.type)}`);
    }

    const callId = message.call_id;
    if (!Number.isSafeInteger(callId) || callId < 0) {
      throw new TypeError("library.invoke.call_id must be a non-negative safe integer");
    }
    const exportName = assertString(message.export, "library.invoke.export");
    const operation = assertString(message.operation, "library.invoke.operation");

    try {
      const result = await invoke(Object.freeze({
        package: identity.package.name,
        export: exportName,
        operation,
        payload: decodePayload(message.payload ?? []),
        rbe,
        session
      }));
      writeLibraryFrame({
        type: "library.reply",
        call_id: callId,
        ok: true,
        payload: Array.from(encodePayload(result)),
        error: null
      }, outputFd);
    } catch (error) {
      writeLibraryFrame({
        type: "library.reply",
        call_id: callId,
        ok: false,
        payload: [],
        error: safeError(error)
      }, outputFd);
    }
  }
}
