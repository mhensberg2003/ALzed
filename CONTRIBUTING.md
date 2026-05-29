# Contributing to ALzed

Thanks for your interest! ALzed is a community project bringing AL / Business
Central support to [Zed](https://zed.dev). Contributions of all sizes are
welcome — bug reports, snippet additions, protocol notes, and code.

## Ways to help

- **Report bugs** with the [issue template](.github/ISSUE_TEMPLATE/bug_report.yml).
  The OS + AL server version fields matter — the bridge handshake is
  version-sensitive.
- **Add or fix snippets** in [`crates/bridge/snippets/al_snippets.json`](crates/bridge/snippets/al_snippets.json).
- **Improve protocol notes** in [`docs/al-protocol.md`](docs/al-protocol.md) —
  the AL server's `al/*` methods are reverse-engineered, so corrections help.
- **Tackle a planned feature** (debug/DAP, test runner — see the Status table
  in the README).

## Project layout

See the [Repo layout](README.md#repo-layout) section. In short: a sandboxed
Zed WASM `extension` that launches a native `bridge` binary, which proxies LSP
to Microsoft's AL Language Server.

## Dev setup

Follow [Install by building from source](README.md#install-by-building-from-source).
For bridge work, `RUST_LOG=alzed_bridge=trace` shows every LSP frame.

## Pull requests

1. Keep changes focused; one concern per PR.
2. Run `cargo fmt` to keep diffs clean; `cargo clippy` should pass with no
   warnings (CI runs the bridge build + clippy on every PR).
3. Build the bridge (`cargo build --release --manifest-path crates/bridge/Cargo.toml`)
   and the extension (`cargo build --target wasm32-wasip2 ...`) before pushing.
4. Describe what you tested and on which OS + AL server version.

## Scope & legalities

ALzed never bundles or redistributes Microsoft's proprietary AL Language
Server — it only talks to the copy a user installs via the official VS Code AL
extension. Please keep contributions within that boundary. See the
[Trademarks and disclaimer](README.md#trademarks-and-disclaimer) section.

## License

By contributing, you agree your work is licensed under the project's
[MIT License](LICENSE).
