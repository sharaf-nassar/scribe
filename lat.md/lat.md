# Scribe

A GPU-accelerated terminal emulator with a client-server architecture and first-class AI process awareness.

- [[architecture]] — System overview, crate map, and data flow
- [[common]] — Shared types: IPC framing, identity IDs, errors, screen snapshots, config, theme system, and socket paths
- [[protocol]] — IPC messages, screen snapshots, identity types, and AI state
- [[server]] — PTY sessions, workspaces, handoff, and updater
- [[client]] — Panes, layout, input, IPC, selection, and UI chrome
- [[rendering]] — Glyph atlas, wgpu pipeline, colour palette, and box drawing
- [[pty]] — Async PTY I/O, OSC interception, and metadata parsing
- [[settings]] — Webview config editor, key paths, and singleton
- [[test]] — Integration test harness: PTY capture, IPC helpers, assertion utilities, and screenshot rendering
