# Draft Desk: agent instructions

Agents hand mail drafts to Draft Desk; a person reviews them and sends them from the app. Workspace: `core` (draft
format, queue, sending behind the `send` feature), `cli` (the `draftdesk` command line, which can never send) and
`macos` (the AppKit app, the only package that turns `send` on).

## Build and test

- `./build.sh --tests` is the gate before every commit: format check, clippy with `-D warnings`, the feature guard
  and the tests of every package. The other modes are listed at the top of `build.sh`. `examples/check.py` is the
  example check command: Python 3.9 or newer, standard library only. Its tests: `python3 -B examples/test_check.py`
  (part of `./build.sh --tests`).
- Build one package per cargo call (`-p draftdesk-cli`, `-p draftdesk-macos`): cargo unifies features per call, and
  built together with the app the command line would get `send`.
- One test: `cargo test -p draftdesk-core --features send <name>`. The AppKit tests run in their own harness on the
  main thread: `cargo test -p draftdesk-macos --test appkit -- <name>`. Their windows stay invisible and let clicks
  through (`keep_windows_invisible`), because they run while someone works at the same Mac; a new kind of window
  must stay invisible too, and only a check that truly needs the screen may show one, once.
- Tests use a temporary root (`DRAFTDESK_HOME`) and a secrets file (`DRAFTDESK_TEST_SECRETS_FILE`, debug builds
  only), so the real keychain and the user's queue stay out of reach.
- The core stays portable: CI builds and tests it on Linux; Mac-only code sits behind `cfg(target_os = "macos")`.

## Rules the guards check (`core/tests/guards.rs`)

- Sending code compiles only with the core's `send` feature.
- Program names differ beyond case (the Mac file system ignores case): `draftdesk`, `draftdesk-app`.
- The bundle id lives once: `APP_BUNDLE_ID` in `core/src/lib.rs`, which `build.sh` reads.
- Public repository: example data only (`example.org` addresses, invented names, no real paths or accounts).
- `./build.sh --install` signs with a named identity (`DRAFTDESK_SIGN_ID`).

## AppKit gotchas

- Methods inside `define_class!` take no `?` and no early `return`: the macro rewrites the body.
- `NSApplication::terminate` belongs outside `with_app`.
- Text automatics (a period after two spaces, smart quotes) show only with real key presses; synthetic events miss
  them.
- Keep one registered bundle per bundle id: macOS opens apps by id and may pick a stale copy from `target/`.

## Code and commits

- Test first: a failing test, then the change that makes it pass.
- A deliberate shortcut gets a `ponytail:` comment naming its limit and the way up.
- Stage files by name; commit messages in English, in the style of `git log`.
