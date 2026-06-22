# Contributing

Abacus favors a small, inspectable core. New features should strengthen the coding loop, setup, safety, portability, or reliability without adding a required service.

Before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Add regression tests for behavior changes. Keep provider-specific behavior behind the existing OpenAI-compatible boundary where possible, and preserve session/config forwards compatibility.
