# The OpenSession standard

After reverse-engineering seventeen different transcript formats, one thing was obvious: they're all
*almost the same thing* underneath. **OpenSession** is the format they should have agreed on — a
small, honest, harness-neutral interchange format. clustervision's internal IR is its reference
implementation, and `.opensession.json` is a first-class import/export format in the app and CLI.

The full spec lives at **[docs/OPENSESSION.md](https://github.com/emberian/clustervision/blob/main/docs/OPENSESSION.md)**. The essentials:

## The shape

```jsonc
{
  "openSession": "0.2",
  "id": "…",
  "harness": "claude",            // origin hint, not identity
  "title": "…",
  "cwd": "/Users/you/project",    // metadata, NOT identity (see below)
  "model": "claude-opus-4-…",
  "messages": [
    {
      "role": "user",            // user · assistant · system · tool
      "timestamp": "2026-…Z",
      "content": [               // an ordered list of typed blocks
        { "kind": "text", "text": "…" },
        { "kind": "thinking", "text": "…", "signature": "…", "redacted": false },
        { "kind": "toolUse", "id": "…", "name": "run_shell", "input": { … } },
        { "kind": "toolResult", "toolUseId": "…", "content": "…", "isError": false },
        { "kind": "file", "mime": "…", "path": "…" },
        { "kind": "image", "mediaType": "image/png", "dataRef": "…" }
      ]
    }
  ]
}
```

## The one heresy: *cwd is metadata, not identity*

Most harnesses key a session to the exact directory it ran in, so moving a project loses the thread.
OpenSession records `cwd` as a plain field. A session is its **messages** — it can be read, ported,
and resumed anywhere. This is what makes [porting](cli.md) and [cross-harness conversion](conversion.md)
possible at all.

## Why you'd care

If you ship a harness: emit OpenSession and your users' sessions become portable *by construction* —
every other tool can read, search, and convert them. 🤝
