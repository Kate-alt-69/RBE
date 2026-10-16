/** KA device login for a desktop app's trusted main process. The browser owns
 * credential entry/account selection; this client never contains a service
 * secret and never writes tokens to browser storage or a plaintext file. */
export class KastrickAccountClient {
	#authority;
	#serviceId;
	#fetch;
	#store;
	#credential;
	constructor({ authority, serviceId, credentialStore, fetch: transport = globalThis.fetch }) {
		const url = new URL(authority);
		const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
		if ((url.protocol !== "https:" && !(url.protocol === "http:" && local)) || url.username || url.password || url.search || url.hash || url.pathname !== "/") throw new TypeError("KA authority must be an HTTPS origin; HTTP loopback is allowed for development");
		if (typeof serviceId !== "string" || !/^[a-z0-9_-]{1,64}$/.test(serviceId)) throw new TypeError("Invalid registered KA desktop service ID");
		if (typeof transport !== "function") throw new TypeError("KA requires an HTTP transport");
		if (credentialStore && !["read", "write", "remove"].every(name => typeof credentialStore[name] === "function")) throw new TypeError("KA credentialStore must supply read, write and remove backed by the native OS credential store");
		this.#authority = url; this.#serviceId = serviceId; this.#fetch = transport; this.#store = credentialStore;
	}
	get accountUrl() { return new URL("/account", this.#authority).href; }
	get #storeKey() { return `ka:${this.#authority.origin}:${this.#serviceId}`; }
	async #request(path, { form, body, token, signal } = {}) {
		const headers = { "Content-Type": form ? "application/x-www-form-urlencoded" : "application/json" };
		if (token) headers.Authorization = `Bearer ${token}`;
		const response = await this.#fetch(new URL(path, this.#authority), {
			method: "POST", headers, body: form ? new URLSearchParams(form).toString() : JSON.stringify(body),
			redirect: "error", signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(30000)]) : AbortSignal.timeout(30000),
		});
		const data = await response.json();
		if (!data || typeof data !== "object" || Array.isArray(data)) throw new Error("KA returned an invalid response");
		return { ok: response.ok, status: response.status, data };
	}
	async start({ signal } = {}) {
		const result = await this.#request("/v1/device/authorize", { form: { client_id: this.#serviceId }, signal });
		if (!result.ok) throw new Error(result.data.error_description || "Could not start KA desktop login");
		const data = result.data;
		if (!/^[a-f0-9]{64}$/.test(data.device_code) || !/^[A-F0-9]{4}-[A-F0-9]{4}$/.test(data.user_code) || !Number.isSafeInteger(data.expires_in) || data.expires_in <= 0 || data.expires_in > 600 || !Number.isSafeInteger(data.interval) || data.interval < 1 || data.interval > 300) throw new Error("KA returned an invalid device request");
		const uri = new URL(data.verification_uri); const complete = new URL(data.verification_uri_complete);
		for (const url of [uri, complete]) if (url.origin !== this.#authority.origin || url.pathname !== "/login/device" || url.username || url.password || url.hash) throw new Error("KA returned an untrusted browser URL");
		if (uri.search || complete.searchParams.get("user_code") !== data.user_code) throw new Error("KA returned a mismatched device verification code");
		return Object.freeze({ deviceCode: data.device_code, userCode: data.user_code, verificationUri: uri.href, verificationUriComplete: complete.href, expiresAt: Math.floor(Date.now() / 1000) + data.expires_in, intervalSeconds: data.interval });
	}
	/** Poll once from the main process. Wait intervalSeconds between calls and
	 * increase it by five seconds after slow_down, as required by RFC 8628. */
	async poll(device, { signal } = {}) {
		if (!device || !/^[a-f0-9]{64}$/.test(device.deviceCode) || !Number.isSafeInteger(device.expiresAt)) throw new TypeError("Invalid KA device request");
		if (device.expiresAt <= Math.floor(Date.now() / 1000)) return { state: "expired" };
		const result = await this.#request("/v1/device/token", { form: { client_id: this.#serviceId, device_code: device.deviceCode, grant_type: "urn:ietf:params:oauth:grant-type:device_code" }, signal });
		if (!result.ok) {
			const state = { authorization_pending: "pending", slow_down: "slow_down", access_denied: "denied", expired_token: "expired" }[result.data.error];
			if (state) return { state };
			throw new Error(result.data.error_description || "KA desktop login failed");
		}
		const body = result.data;
		if (body.token_type !== "Bearer" || body.service_id !== this.#serviceId || !/^[a-f0-9]{64}$/.test(body.access_token) || !Number.isSafeInteger(body.expires_in) || body.expires_in <= 0 || body.expires_in > 30 * 86400 || typeof body.user_id !== "string" || !body.user_id) throw new Error("KA returned an invalid scoped session");
		const credential = Object.freeze({ authority: this.#authority.origin, serviceId: this.#serviceId, userId: body.user_id, accessToken: body.access_token, expiresAt: Math.floor(Date.now() / 1000) + body.expires_in });
		if (this.#store) {
			try { await this.#store.write(this.#storeKey, credential); }
			catch {
				try { await this.#request("/v1/device/revoke", { token: credential.accessToken, body: { serviceId: this.#serviceId } }); } catch { /* Expiry remains a fallback if the provider is unavailable. */ }
				throw new Error("Could not save the KA session in the native credential store; start login again");
			}
		}
		this.#credential = credential;
		return { state: "connected", userId: credential.userId, expiresAt: credential.expiresAt };
	}
	async signIn({ onDevice, onPending, signal } = {}) {
		if (typeof onDevice !== "function") throw new TypeError("KA signIn needs onDevice to show the code and open the system browser");
		const device = await this.start({ signal });
		// Only non-secret fields reach the UI callback. Keep deviceCode out of
		// renderer IPC, logs, and analytics as well as account-session tokens.
		await onDevice({ userCode: device.userCode, verificationUri: device.verificationUri, verificationUriComplete: device.verificationUriComplete, expiresAt: device.expiresAt });
		let interval = device.intervalSeconds;
		while (Math.floor(Date.now() / 1000) < device.expiresAt) {
			await wait(interval * 1000, signal);
			const result = await this.poll(device, { signal });
			if (result.state === "connected") return result;
			if (result.state === "denied") throw new Error("KA desktop connection was cancelled");
			if (result.state === "expired") break;
			if (result.state === "slow_down") interval += 5;
			onPending?.(result.state);
		}
		throw new Error("KA desktop login expired; start again");
	}
	async resume() {
		if (this.#credential?.expiresAt > Math.floor(Date.now() / 1000)) return true;
		this.#credential = undefined;
		if (!this.#store) return false;
		const value = await this.#store.read(this.#storeKey);
		if (!value) return false;
		if (value.authority !== this.#authority.origin || value.serviceId !== this.#serviceId || !/^[a-f0-9]{64}$/.test(value.accessToken) || !Number.isSafeInteger(value.expiresAt) || value.expiresAt <= Math.floor(Date.now() / 1000)) { await this.#store.remove(this.#storeKey); return false; }
		this.#credential = value; return true;
	}
	async #token() { if (!await this.resume()) throw new Error("Sign in to your KA desktop account first"); return this.#credential.accessToken; }
	async profile({ signal } = {}) {
		return this.#profile("getProfile", {}, signal);
	}
	async updateProfile({ displayName, avatarTone, expectedRevision }, { signal } = {}) {
		return this.#profile("updateProfile", { displayName, avatarTone, expectedRevision }, signal);
	}
	async #profile(action, edits, signal) {
		const result = await this.#request("/v1/device/profile", { token: await this.#token(), body: { serviceId: this.#serviceId, action, ...edits }, signal });
		if (result.status === 401) {
			// A dashboard disconnect invalidates every session for this service.
			// Forget the rejected credential before requesting another login.
			this.#credential = undefined;
			await this.#store?.remove(this.#storeKey);
			throw new Error("Your KA session expired or was disconnected; sign in again");
		}
		if (!result.ok || result.data.ok !== true) throw new Error(result.data.error === "profile_changed" ? "Your profile changed elsewhere; refresh it before saving" : "Could not read or update your KA profile");
		const profile = result.data.profile;
		if (!profile || typeof profile.displayName !== "string" || !Number.isSafeInteger(profile.revision)) throw new Error("KA returned an invalid profile");
		const avatar = new URL(result.data.avatarUrl);
		if (avatar.origin !== this.#authority.origin || !avatar.pathname.startsWith("/avatars/") || avatar.username || avatar.password) throw new Error("KA returned an invalid generated avatar");
		return { profile, avatarUrl: avatar.href, storage: result.data.storage };
	}
	async signOut({ signal } = {}) {
		if (await this.resume()) {
			const result = await this.#request("/v1/device/revoke", { token: this.#credential.accessToken, body: { serviceId: this.#serviceId }, signal });
			if (!result.ok && result.status !== 401) throw new Error("Could not revoke the KA session; try again");
		}
		await this.#store?.remove(this.#storeKey); this.#credential = undefined;
	}
}
function wait(milliseconds, signal) {
	return new Promise((resolve, reject) => {
		if (signal?.aborted) return reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
		const timer = setTimeout(() => { signal?.removeEventListener("abort", abort); resolve(); }, milliseconds);
		function abort() { clearTimeout(timer); signal?.removeEventListener("abort", abort); reject(signal.reason ?? new DOMException("Aborted", "AbortError")); }
		signal?.addEventListener("abort", abort, { once: true });
	});
}
