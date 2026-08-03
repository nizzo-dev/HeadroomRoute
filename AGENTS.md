# Repository Guidelines

## Project Structure & Module Organization

This repository builds one Rust 2024 Windows binary. `src/main.rs` is the entry point and wires together the tray UI, local proxy, and background workers. Keep responsibilities in the existing modules: configuration and client updates in `src/config.rs`, shared types in `src/model.rs`, HTTP routing in `src/proxy.rs`, managed Python/Headroom setup in `src/runtime.rs`, persistence helpers in `src/sqlite.rs` and `src/state.rs`, Windows UI in `src/tray.rs`, and long-running tasks in `src/worker.rs`.

Unit tests live beside their implementation under `#[cfg(test)]`. Build artifacts are generated in `target/` and release executables in `dist/`; neither should be committed. Root PowerShell scripts provide the supported Windows build and install workflows.

## Build, Test, and Development Commands

- `cargo check` performs a fast compile check during development.
- `cargo test` runs all inline unit tests.
- `cargo fmt -- --check` verifies standard Rust formatting.
- `cargo clippy --all-targets -- -D warnings` catches common Rust issues and treats warnings as failures.
- `.\Build.ps1` checks, tests, and creates optimized binaries under `dist\`.
- `.\Install.ps1 -StartNow` copies a built binary to `%LOCALAPPDATA%\HeadroomRoute` and launches it.
- `dist\HeadroomRoute.exe --doctor` prints a redacted diagnostic report for local troubleshooting.

Use Rust stable. The build script automatically uses a cached `cargo-xwin` SDK when the normal Visual Studio C++ toolchain is unavailable.

## Coding Style & Naming Conventions

Run `cargo fmt` before submitting changes. Use four-space indentation, `snake_case` for functions/modules, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer small module-local helpers and propagate recoverable failures with `anyhow::Result` and contextual errors. Isolate `unsafe` Windows API calls, documenting non-obvious lifetime or ownership assumptions.

## Testing Guidelines

Add focused unit tests in the module being changed; name tests after observable behavior, such as `restores_original_config`. Cover configuration migration, routing decisions, state transitions, and parsing edge cases. There is no stated coverage threshold, but every bug fix should include a regression test where practical. Run `cargo test` and `.\Build.ps1` before opening a PR.

## Commit & Pull Request Guidelines

History currently uses concise, imperative summaries (for example, `Initial release of HeadroomRoute 0.3.0`). Keep commits narrowly scoped and describe the user-visible outcome. PRs should explain motivation, testing performed, and Windows/runtime implications; link related issues and include tray screenshots for UI changes. Never commit API keys, local `%LOCALAPPDATA%` state, logs, or generated binaries.

## Versioning & Packaging

Before every package or release build, propose a version based on the change scope and ask the user to confirm it. Small fixes increment the patch number (for example, `0.6.0` to `0.6.1`); only larger feature or compatibility changes should increment an earlier SemVer component. Never choose or publish a packaging version without the user's confirmation.
