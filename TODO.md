# TODO

## Known Limitations

- [ ] **Oversized individual turns**: A single turn containing an enormous tool result (e.g. a large file read) can itself exceed the context limit. Rotation won't help since the turn can't be split. Needs chunking or truncation of tool result content before it enters history.
- [ ] Setup push notifications on mobile to let user know when something is finished.
