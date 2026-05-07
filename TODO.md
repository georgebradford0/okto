# TODO

- [ ] Add extensive logging
- [ ] mcp import mcp.json is not working (might be fixed)
- [ ] check if api keys used for starting up lair are visible in deployment data, move to secrets
- [ ] Setup background tasks
- [ ] More robust pod readiness waiting — child pods have no readiness probe so `wait_for_deployment_ready` returns as soon as the process starts, not when the server is actually listening. Add a `/health` readiness probe to child deployments (same as lair) so reload and create reliably wait until the pod is ready to accept connections.

# POSSIBLY TODO
- [ ] Setup push notifications on mobile to let user know when something is finished.
- [ ] Add client pubkey allowlist on server/master — currently any client that knows the server pubkey+host+port can connect.
