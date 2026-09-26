# Eve.aic

Eve.aic is a Rust-based, plugin-first runtime designed as the foundation for a future autonomous cognitive architecture.

The first phase focuses on a small and reliable runtime kernel:

```text
Plugin Registry
    ↓
Dependency Resolution
    ↓
Plugin Lifecycle
    ↓
Context
    ↓
Event / Service / State / Task
    ↓
Cleanup / Shutdown / Recovery
```

The project design and execution documents are in [`docs/`](./docs/README.md).

