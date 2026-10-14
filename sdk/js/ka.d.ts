export interface KastrickCredential {
	authority: string;
	serviceId: string;
	userId: string;
	accessToken: string;
	expiresAt: number;
}
/** Implement this interface in the trusted desktop main process using the OS
 * credential vault. Never bridge credential values to renderer JavaScript. */
export interface KastrickCredentialStore {
	read(key: string): Promise<KastrickCredential | undefined | null>;
	write(key: string, credential: KastrickCredential): Promise<void>;
	remove(key: string): Promise<void>;
}
export interface KastrickDevice {
	deviceCode: string;
	userCode: string;
	verificationUri: string;
	verificationUriComplete: string;
	expiresAt: number;
	intervalSeconds: number;
}
export interface KastrickProfile {
	userId: string;
	displayName: string;
	email: string | null;
	avatarSeed: string;
	avatarTone: "violet" | "cyan" | "rose" | "amber";
	revision: number;
}
export type KastrickPoll = { state: "pending" | "slow_down" | "denied" | "expired" } | { state: "connected"; userId: string; expiresAt: number };
export class KastrickAccountClient {
	constructor(options: { authority: string; serviceId: string; credentialStore?: KastrickCredentialStore; fetch?: typeof fetch });
	readonly accountUrl: string;
	start(options?: { signal?: AbortSignal }): Promise<Readonly<KastrickDevice>>;
	poll(device: KastrickDevice, options?: { signal?: AbortSignal }): Promise<KastrickPoll>;
	signIn(options: { onDevice(device: Omit<KastrickDevice, "deviceCode" | "intervalSeconds">): void | Promise<void>; onPending?(state: "pending" | "slow_down"): void; signal?: AbortSignal }): Promise<Extract<KastrickPoll, { state: "connected" }>>;
	resume(): Promise<boolean>;
	profile(options?: { signal?: AbortSignal }): Promise<{ profile: KastrickProfile; avatarUrl: string; storage: { managedQuotaBytes: number; managedUsedBytes: number; managedRemainingBytes: number } }>;
	updateProfile(edits: { displayName: string; avatarTone: KastrickProfile["avatarTone"]; expectedRevision: number }, options?: { signal?: AbortSignal }): ReturnType<KastrickAccountClient["profile"]>;
	signOut(options?: { signal?: AbortSignal }): Promise<void>;
}
