export const LIBRARY_ABI_VERSION: 1;
export const SDK_VERSION: string;

export type HostRequest = {
  capability: string;
  target: string;
  operation: string;
  payload: unknown;
};

export type BatchResult<T = unknown> =
  | { ok: true; value: T }
  | { ok: false; error: unknown };

export interface HostBridge {
  call(request: HostRequest): unknown | Promise<unknown>;
}

export interface HostInterceptor {
  before?(request: HostRequest): HostRequest | void;
  after?(request: HostRequest, reply: unknown): unknown | void;
  onError?(request: HostRequest, error: unknown): unknown | void;
}

export type LibraryDescriptor = Readonly<{
  name: string;
  version: string;
  abiMin: number;
  abiMax: number;
}>;

export declare const capability: Readonly<Record<string, string>>;

export declare function libraryDescriptor(input: {
  name: string;
  version: string;
  abiMin?: number;
  abiMax?: number;
}): LibraryDescriptor;

export declare class InterceptedBridge implements HostBridge {
  constructor(bridge: HostBridge, interceptors?: readonly HostInterceptor[]);
  readonly bridge: HostBridge;
  readonly interceptors: readonly HostInterceptor[];
  call(request: HostRequest): unknown | Promise<unknown>;
}

export declare class CapabilityClient {
  constructor(bridge: HostBridge, capabilityId: string, target?: string);
  readonly bridge: HostBridge;
  readonly capabilityId: string;
  readonly target: string;
  request(operation: string, payload?: unknown): HostRequest;
  call(operation: string, payload?: unknown): unknown | Promise<unknown>;
  retarget(target: string): CapabilityClient;
  intercept(...interceptors: HostInterceptor[]): CapabilityClient;
}

export declare class AdvancedClient {
  constructor(bridge: HostBridge);
  capability(capabilityId: string, target?: string): CapabilityClient;
  request(capabilityId: string, target: string, operation: string, payload?: unknown): HostRequest;
  send(request: HostRequest): unknown | Promise<unknown>;
  batch(requests: readonly HostRequest[]): Promise<BatchResult[]>;
  intercept(...interceptors: HostInterceptor[]): AdvancedClient;
  hostBridge(): HostBridge;
}

export declare class RbeSdk {
  constructor(bridge: HostBridge);
  call(request: HostRequest): unknown | Promise<unknown>;
  capability(capabilityId: string, target?: string): CapabilityClient;
  advanced(): AdvancedClient;
  intercept(...interceptors: HostInterceptor[]): RbeSdk;
  hostBridge(): HostBridge;
  net(): {
    sublibrary(name: string): CapabilityClient;
    http(): CapabilityClient;
    p2p(): CapabilityClient;
    mask(): CapabilityClient;
  };
  router(): {
    inspect(operation: string, payload?: unknown): unknown | Promise<unknown>;
    register(operation: string, payload?: unknown): unknown | Promise<unknown>;
  };
  storage(): CapabilityClient;
  crypto(): CapabilityClient;
}
