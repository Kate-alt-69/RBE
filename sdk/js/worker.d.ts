import type { HostBridge, HostRequest, HostSessionInfo, RbeSdk } from "./index.js";

export const LIBRARY_PROTOCOL_VERSION: 1;
export const MAX_LIBRARY_FRAME_BYTES: number;
export const MAX_LIBRARY_PAYLOAD_BYTES: number;

export type LibraryWorkerIdentity = Readonly<{
  package: Readonly<{
    name: string;
    version: string;
    artifactSha256: string;
  }>;
  runtime: Readonly<{
    kind: string;
    version: string;
  }>;
  sdk?: Readonly<{
    language?: string;
    name?: string;
    version?: string;
  }>;
  abiMin?: number;
  abiMax?: number;
}>;

export type LibraryInvocation = Readonly<{
  package: string;
  export: string;
  operation: string;
  payload: unknown;
  rbe: RbeSdk;
  session: HostSessionInfo;
}>;

export type LibraryWorkerOptions = Readonly<{
  identity: LibraryWorkerIdentity;
  invoke(invocation: LibraryInvocation): unknown | Promise<unknown>;
  inputFd?: number;
  outputFd?: number;
}>;

export declare function readLibraryFrame(fd?: number): Record<string, unknown> | null;
export declare function writeLibraryFrame(value: Record<string, unknown>, fd?: number): void;

export declare class LibraryWorkerBridge implements HostBridge {
  constructor(options: { inputFd?: number; outputFd?: number; session: HostSessionInfo });
  readonly inputFd: number;
  readonly outputFd: number;
  readonly session: HostSessionInfo;
  sessionInfo(): HostSessionInfo;
  call(request: HostRequest): unknown;
}

export declare function runLibraryWorker(options: LibraryWorkerOptions): Promise<void>;
