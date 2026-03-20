# TODO

## Known Limitations

- [ ] **Oversized individual turns**: A single turn containing an enormous tool result (e.g. a large file read) can itself exceed the context limit. Rotation won't help since the turn can't be split. Needs chunking or truncation of tool result content before it enters history.
- [ ] Make sure entire environment is injected into Tauri app when run so bash can access what it needs
