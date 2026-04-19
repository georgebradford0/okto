# TODO

- [ ] Setup a more robust system for managing children containers
- [ ] Change core to give Claude root access to docker container
- [ ] Make README.md professional for sharing with others
- [ ] Setup push notifications on mobile to let user know when something is finished.
- [ ] Add client pubkey allowlist on server/master — currently any client that knows the server pubkey+host+port can connect.
- [ ] Interrupt during tool call — stop button sets aborted flag but tool subprocess runs to completion; need to plumb aborted into tool runner and kill subprocess mid-execution.
