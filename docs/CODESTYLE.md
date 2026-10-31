# Code style

## Formatting and lints

- `cargo fmt` with [`rustfmt.toml`](../rustfmt.toml): edition 2024, Unix newlines, field-init and
  `?` shorthand.
- `cargo clippy --all-targets --all-features -- -D warnings` with [`clippy.toml`](../clippy.toml);
  a warning fails CI.
- Markdown follows [`.markdownlint-cli2.jsonc`](../.markdownlint-cli2.jsonc).

## Naming

- Functions say what they do to what: `render_conflist`, `sync_conflist`, `write_conflist`.
- Tests are named for the behaviour they pin, as a sentence: `rejects_an_empty_list`,
  `recovers_from_a_request_the_api_never_answers`.
- Constants carry their unit or role in the name when the type does not: `MEMORY_REPORT_PERIOD`,
  `watch_secs`.

## Error handling

- `anyhow::Result` throughout; every `?` that crosses an I/O or API boundary gets a `.context()`
  naming what failed and on what.
- An error the process cannot recover from in place is returned up to `main`, which exits non-zero
  so the kubelet restarts the pod. Errors are not logged and then dropped.
- Transient API errors are left to the watcher's backoff, counted, and logged at `warn`.
- `unwrap`/`expect` only in tests.

## Logging

`tracing` with structured fields (`tracing::info!(?pod_cidrs, path = %path.display(), "...")`),
the message a fixed phrase. `info` for state changes, `warn` for retried errors, `debug` for
the periodic memory report and for waiting states.

## Layout

- One module per concern: `conflist.rs` renders, `memstats.rs` reports memory, `main.rs` wires the
  watch to the file.
- Pure logic stays free of I/O so it can be tested directly; I/O takes its path or client as a
  parameter so tests can point it at a temp dir or a local stand-in API server.
- Tests sit in a `#[cfg(test)] mod tests` at the end of the file they test.

## Comments

Code says what it does; a comment says only why it has to be that way - an external constraint
or a non-obvious consequence, including how an external component behaves. No history of how the
code got here, and no pointers to files or functions in other repositories.
