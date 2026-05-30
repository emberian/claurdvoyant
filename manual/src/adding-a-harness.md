# Adding a harness

Got a coding agent we don't support yet? Adding one is a single new module implementing the
[`Adapter`](architecture.md) trait. The full walkthrough is in
**[ADDING_HARNESS.md](https://github.com/emberian/claurdvoyant/blob/main/ADDING_HARNESS.md)**; the shape:

1. **`discover()`** — cheaply enumerate sessions on disk into `SessionRef`s (id, path, cwd, title,
   timestamps, a message count). No full parse.
2. **`parse()`** — turn one `SessionRef` into the unified [IR](architecture.md)
   (`Session → Message → Block`). This is the only *required* heavy lifting.
3. **`emit()`** *(optional)* — write the IR back into the harness's native on-disk format, making it
   a [conversion target](conversion.md). Pair it with a round-trip test (emit → re-parse → compare).
4. Register it in `harness::all()` and add a `Harness` enum variant.

The fastest way to get it right: **send us your transcripts.** We can only test what we can see, and
historical format variants are an explicit goal. Open an issue or a PR with a few real (redacted, if
you like — `cv redact` helps) session files. 💜
