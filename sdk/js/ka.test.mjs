import test from "node:test";
import assert from "node:assert/strict";
import { KastrickAccountClient } from "./ka.js";
const authority = "https://accounts.example";
const serviceId = "desktop-sketch";
const code = "a".repeat(64); const token = "b".repeat(64);
const start = { device_code: code, user_code: "ABCD-1234", verification_uri: `${authority}/login/device`, verification_uri_complete: `${authority}/login/device?user_code=ABCD-1234`, expires_in: 600, interval: 5 };
const connected = { access_token: token, token_type: "Bearer", service_id: serviceId, user_id: "usr_test", expires_in: 3600 };
function fixture(replies, store) {
	const calls = [];
	const client = new KastrickAccountClient({ authority, serviceId, credentialStore: store, fetch: async (url, init) => {
		calls.push({ url: String(url), ...init }); const reply = replies.shift();
		assert.ok(reply, "unexpected HTTP request"); return new Response(JSON.stringify(reply.body ?? reply), { status: reply.status ?? 200 });
	} });
	return { client, calls };
}
test("desktop starts use public client IDs and validate browser destinations", async () => {
	const { client, calls } = fixture([start]); const device = await client.start();
	assert.equal(device.deviceCode, code); assert.equal(device.userCode, "ABCD-1234");
	assert.equal(calls[0].headers.Authorization, undefined);
	assert.equal(calls[0].body, "client_id=desktop-sketch");
	const bad = fixture([{ ...start, verification_uri_complete: "https://evil.example/login/device?user_code=ABCD-1234" }]);
	await assert.rejects(() => bad.client.start(), /untrusted/);
	assert.throws(() => new KastrickAccountClient({ authority: "http://remote.example", serviceId }), /HTTPS/);
});
test("pending, slow_down, denial and expiry do not save credentials", async () => {
	const replies = [start, ...["authorization_pending", "slow_down", "access_denied", "expired_token"].map(error => ({ status: 400, body: { error } }))];
	const { client } = fixture(replies); const device = await client.start();
	for (const state of ["pending", "slow_down", "denied", "expired"]) assert.equal((await client.poll(device)).state, state);
	assert.equal(await client.resume(), false);
});
test("tokens remain in main-process memory or the supplied credential vault", async () => {
	const writes = new Map();
	const store = { read: async key => writes.get(key), write: async (key, value) => writes.set(key, value), remove: async key => writes.delete(key) };
	const profile = { ok: true, profile: { userId: "usr_test", displayName: "Ada", revision: 1 }, avatarUrl: `${authority}/avatars/${"c".repeat(64)}?tone=cyan`, storage: {} };
	const { client, calls } = fixture([start, connected, profile, { ok: true }], store);
	const device = await client.start(); const result = await client.poll(device);
	assert.equal(result.state, "connected"); assert.equal(result.accessToken, undefined); assert.equal(JSON.stringify(client).includes(token), false);
	assert.equal(writes.size, 1); assert.equal((await client.profile()).profile.displayName, "Ada");
	assert.equal(calls[2].headers.Authorization, `Bearer ${token}`);
	await client.signOut(); assert.equal(writes.size, 0); assert.equal(await client.resume(), false);
});
test("a failed secure store write revokes the newly issued token", async () => {
	const store = { read: async () => undefined, write: async () => { throw new Error("vault unavailable"); }, remove: async () => {} };
	const { client, calls } = fixture([start, connected, { ok: true }], store);
	const device = await client.start(); await assert.rejects(() => client.poll(device), /native credential store/);
	assert.ok(calls[2].url.endsWith("/v1/device/revoke")); assert.equal(await client.resume(), false);
});
test("service mismatch and stale profile edits fail without replacing identity", async () => {
	const bad = fixture([start, { ...connected, service_id: "other-app" }]); const device = await bad.client.start();
	await assert.rejects(() => bad.client.poll(device), /scoped session/);
	const { client } = fixture([start, connected, { status: 409, body: { ok: false, error: "profile_changed" } }]);
	await client.poll(await client.start()); await assert.rejects(() => client.updateProfile({ displayName: "Draft", avatarTone: "cyan", expectedRevision: 1 }), /changed elsewhere/);
});
test("cancelling login never passes a device secret to the UI callback", async () => {
	const { client } = fixture([start]); const controller = new AbortController();
	let shown;
	await assert.rejects(() => client.signIn({ signal: controller.signal, onDevice: data => { shown = data; controller.abort(); } }), /abort/i);
	assert.equal(shown.userCode, "ABCD-1234"); assert.equal(shown.deviceCode, undefined); assert.equal(shown.accessToken, undefined);
});
test("a disconnected service forgets its rejected native credential", async () => {
	const writes = new Map();
	const store = { read: async key => writes.get(key), write: async (key, value) => writes.set(key, value), remove: async key => writes.delete(key) };
	const { client } = fixture([start, connected, { status: 401, body: { ok: false } }], store);
	await client.poll(await client.start());
	await assert.rejects(() => client.profile(), /disconnected/);
	assert.equal(writes.size, 0); assert.equal(await client.resume(), false);
});
