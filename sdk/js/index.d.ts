export const LIBRARY_ABI_VERSION: 1;
export const SDK_VERSION: string;

export type HostRequest = {
  capability: string;
  target: string;
  operation: string;
  payload: unknown;
};

export interface HostBridge {
  call(request: HostRequest): unknown | Promise<unknown>;
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

export declare class RbeSdk {
  constructor(bridge: HostBridge);
  call(request: HostRequest): unknown | Promise<unknown>;
  net(): {
    sublibrary(name: string): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
    http(): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
    p2p(): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
    mask(): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
  };
  router(): {
    inspect(operation: string, payload?: unknown): unknown | Promise<unknown>;
    register(operation: string, payload?: unknown): unknown | Promise<unknown>;
  };
  storage(): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
  crypto(): { call(operation: string, payload?: unknown): unknown | Promise<unknown> };
}
