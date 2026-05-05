# TODO

- [ ] mcp import mcp.json is not working
- [ ] check if api keys used for starting up rulyeh are visible in deployment data, move to secrets
- [ ] Remove title and dropdown and use a sidebar instead to list containers
- [ ] Setup background tasks
- [ ] Make README.md professional for sharing with others
- [ ] Setup push notifications on mobile to let user know when something is finished.
- [ ] Add client pubkey allowlist on server/master — currently any client that knows the server pubkey+host+port can connect.
- [ ] `reload --all` explicitly patches image to `latest` but `imagePullPolicy: Always` already handles this on restart — evaluate whether the explicit image patch in `update_and_restart_all` is still needed or can be removed.
