# Contributing

Abacus favors a small, inspectable core. New features should strengthen the coding loop, setup, safety, portability, or reliability without adding a required service.

Before submitting a change:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Add regression tests for behavior changes. Keep provider-specific behavior behind the existing OpenAI-compatible boundary where possible, and preserve session/config forwards compatibility.

## Adding a language

`abacus setup` and `abacus doctor` print from a catalog in `src/i18n.rs`: an
`enum Key`, a table per language under `src/i18n/`, and `i18n::t(key)` at the
call sites. Adding one is three edits:

1. Add a module under `src/i18n/` exporting `TABLE: &[(&str, &str)]`, keyed by
   the ids `Key::id()` returns. Copy `en.rs` and translate the values.
2. Add a variant to `enum Lang`, an arm in `Lang::code`, `Lang::parse`,
   `Lang::table`, and `set_lang`.
3. Run `cargo test --lib`. The catalog tests fail with the exact id that is
   missing, empty, or has a different number of `{}` holes than the English,
   so a half-finished table cannot ship.

Keep ASCII tokens the reader types (`/config`, `abacus doctor`, flag names)
untranslated, and keep `{}` holes in the same order as the English. Format
arguments stay at the call site, so a translation never owns the arguments.

## Verification gate

A change is done when `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test --all-targets` all pass on a clean tree — PASS, HOLD (waiting
on something outside the change, named explicitly), or BLOCK. State the verdict
in the PR; "all tests complete" after running only the unit tests is not one.

Every red check gets an origin: **introduced** (fails because of this change),
**pre_existing** (fails identically at the merge-base in a clean worktree), or
**unknown** (not yet bisected — the change does not land until it is). A
pre-existing failure is only claimed from an observed rerun at the merge-base,
never by inspection. Verification commands must be exact and runnable by anyone;
a model grading its own output is not verification.
