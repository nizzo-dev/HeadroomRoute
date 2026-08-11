# Repository Guidelines

## Scope and Project Map

This repository builds two Rust 2024 Windows x64 binaries:

- `HeadroomRoute` (`src/main.rs`) is the tray application, local proxy, provider
  router, updater, and background-worker host.
- `HeadroomRouteCLI` (`src/cli.rs`) is the terminal/ConPTY wrapper used by
  `hr.cmd` for Codex and Claude commands.

Keep responsibilities in the existing modules. Configuration and provider
discovery live in `src/config.rs` and `src/config/`; shared types are in
`src/model.rs` and `src/model/`; HTTP routing and failover are in
`src/proxy.rs`, `src/proxy/`, and `src/routing_policy.rs`; managed Headroom
environment checks are in `src/runtime.rs` and `src/environment_recovery.rs`;
persistent background tasks are coordinated in `src/worker.rs`;
persistence is in `src/sqlite.rs` and `src/state.rs`; Windows UI and tray
actions are in `src/tray.rs` and `src/tray/`; approval/terminal behavior is in
`src/approval.rs`, `src/approval/`, and `src/notification.rs`; installation and
upgrade support is in `src/updater.rs` and `src/progress.rs`. Add a new module
only when it has a clear ownership boundary.

Supporting files:

- `Build.ps1` performs the supported check, test, isolated-install, and release
  build workflow.
- `Install.ps1` installs, upgrades, and rolls back a build.
- `Test-Install.ps1`, `Test-CliPopup.ps1`, and `Test-ApprovalVisual.ps1` cover
  installation and interactive Windows behavior.
- `README.md`, `COMPATIBILITY.md`, and `RELEASE.md` are user/release-facing
  documentation; update them when behavior, supported versions, or packaging
  changes.

Unit tests live beside their implementation under `#[cfg(test)]`. Build output
is generated in `target/` and `dist/`; neither should be committed. Runtime
state belongs under `%LOCALAPPDATA%\HeadroomRoute`, and CC-Switch data is an
external input, not a repository fixture.

## Prerequisites and Environment

Use Rust stable on Windows x64. `Build.ps1` can configure the cached
`cargo-xwin` SDK when the Visual Studio C++ linker/SDK is unavailable. Use
Windows PowerShell 5.1 for the supported PowerShell scripts. The application
uses an existing user-managed Python/Headroom environment; development and
tests must not install, upgrade, or delete that environment implicitly.

Do not rely on machine-specific paths, credentials, certificates, or an
interactive desktop in unit tests. Tests that require a tray, ConPTY, signing
certificate, or real third-party CLI should be explicit script/integration
checks and documented as Windows-only.

## Build, Test, and Development Commands

When the repository's RTK wrapper is available, prefix shell commands with
`rtk` (for example, `rtk cargo test` and `rtk git status`). For debugging or
commands without an RTK filter, `rtk proxy <command>` is acceptable.

- `cargo check` performs a fast compile check.
- `cargo test` runs all Rust unit tests.
- `cargo fmt -- --check` verifies standard formatting.
- `cargo clippy --all-targets -- -D warnings` treats warnings as errors.
- `./Build.ps1` (or `PowerShell -File .\Build.ps1`) runs checks, tests,
  isolated install tests, and creates versioned release artifacts under `dist\`.
- `./Test-Install.ps1 -Release` exercises install, upgrade, rollback, and
  signature-policy paths in temporary directories.
- `./Test-CliPopup.ps1` and `./Test-ApprovalVisual.ps1` exercise interactive
  approval UI; run them only on a Windows desktop session.
- `./Install.ps1 -StartNow` installs to `%LOCALAPPDATA%\HeadroomRoute` and
  launches the application. Use `-SkipPathUpdate` when a test must not change
  the user PATH.
- `dist\HeadroomRoute.exe --doctor` prints a redacted diagnostic report.

For a normal code change, run `cargo fmt`, `cargo check`, focused tests, then
the full `cargo test` and Clippy checks. Run `Build.ps1` before a release or
when changing packaging, installation, runtime detection, or Windows behavior.

## Implementation and Safety Rules

Use four-space indentation, `snake_case` for functions/modules, `CamelCase` for
types, and `SCREAMING_SNAKE_CASE` for constants. Prefer small module-local
helpers and propagate recoverable failures with `anyhow::Result` plus context.
Isolate `unsafe` Windows API calls and document non-obvious lifetime or
ownership assumptions.

Keep each hand-written source file at or below 500 lines after formatting.
Begin a module split at 400 lines; do not increase an existing legacy exception
above 500 lines without documenting the reason and follow-up plan. Generated
files, lockfiles, build artifacts, and CodeGraph data are excluded from this
limit.

Never commit API keys, tokens, certificates/private keys, `%LOCALAPPDATA%`
state, CC-Switch databases, logs, diagnostic exports, or generated binaries.
Do not broaden a read-only provider import into a write operation without an
explicit feature requirement. Preserve configuration backups and rollback
behavior when changing installation or CLI configuration code.

## Testing Guidelines

Add focused unit tests in the module being changed. Name tests after observable
behavior, such as `restores_original_config`. Cover configuration migration,
provider discovery, routing/failover decisions, state transitions, persistence,
approval parsing, and runtime/version edge cases as applicable. Every bug fix
should include a regression test where practical.

For changes touching Windows integration, also run the smallest relevant
script and record its platform, PowerShell, Rust, Python, and third-party CLI
versions. Do not claim release validation from unit tests alone; consult
`COMPATIBILITY.md` and `RELEASE.md` for the current matrix and artifact checks.

## Commit and Pull Request Guidelines

Use concise, imperative commit subjects (for example, `Restore provider
configuration after failed upgrade`). Keep commits narrowly scoped and avoid
mixing formatting-only churn with behavior changes. A PR should explain the
user-visible motivation, affected configuration/runtime paths, tests run, and
Windows implications. Include tray or approval-popup screenshots for UI
changes, link relevant issues, and call out migration, rollback, or compatibility
impact.

## Versioning and Packaging

Before every package or release build, propose a version based on the change
scope and ask the user to confirm it. Small fixes increment the patch number
(for example, `0.8.3` to `0.8.4`); only larger feature or compatibility changes
should increment an earlier SemVer component. Never choose or publish a
packaging version without user confirmation.

Release work must verify both versioned executables, the ZIP contents, and
SHA-256 checksums. Code signing is optional for ordinary development builds but
must use the explicit `Build.ps1` signing parameters and be independently
verified before claiming a signed release. Never publish artifacts, alter a
user installation, or push tags/releases unless the user has explicitly asked
for that operation and confirmed the proposed version.
