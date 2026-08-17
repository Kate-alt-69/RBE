# Container Runtime Dashboard

The standalone `container` process exposes a read-only observability dashboard.

## Default

```text
http://127.0.0.1:8787/
```

The dashboard is enabled by default when `RBE_CONTAINER_TOKEN` is available. It binds to loopback only by default.

Run explicitly:

```bash
RBE_CONTAINER_TOKEN=dev-token cargo run -p container -- --dashboard-listen 127.0.0.1:8787
```

Disable it with:

```bash
container --no-dashboard
```

Change the listen address with:

```bash
container --dashboard-listen 127.0.0.1:9000
```

## Authentication

Every request requires:

```http
Authorization: Bearer <RBE_CONTAINER_TOKEN>
```

The dashboard does not expose execution, cancellation, restart, or other control mutations. Those remain on the authenticated container control IPC surface.

## Views

The dashboard currently shows:

- container PID and live global queue depth;
- WASM artifact/profile cache counts;
- Environment generation, queue, queue cost, worker count and 100 MiB ephemeral storage budget;
- every Environment's Swamps and their queue/throughput/completion/failure statistics;
- every Worker state, current execution ID, average execution duration, completions and failures;
- recent container events from `container-events.jsonl`;
- the active deny-by-default security policy and Linux enforcement stack.

The event view is intentionally read-only and keeps the most recent 250 valid JSONL events.

## Security model

The default bind address is `127.0.0.1`. Do not expose the dashboard directly to the public network. If remote access is eventually required, put it behind a properly authenticated HTTPS reverse proxy or an equivalent access boundary rather than binding the container dashboard to `0.0.0.0` without additional protection.
