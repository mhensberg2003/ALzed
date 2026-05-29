# Submitting ALzed to the Zed extension registry

Goal: get **AL** into Zed's in-app extension search by adding it to
[`zed-industries/extensions`](https://github.com/zed-industries/extensions).

ALzed's design (a WASM shim that launches a user-installed `alzed-bridge`,
which proxies to Microsoft's user-installed AL Language Server) is an accepted
pattern — the registry rule is "don't *bundle* a language server," not "the LSP
must be open source." Precedents: `veryl`, `slint`, `ocaml`, `crystal`.

## Prerequisites (already satisfied)

- [x] `crates/extension/extension.toml` has `id`, `name`, `version`,
      `schema_version`, `authors`, `repository`, `description`,
      `[language_servers.al]`, `[grammars.al]`.
- [x] A recognized `LICENSE` at the manifest path (`crates/extension/LICENSE`).
- [x] No bundled LSP / no committed `extension.wasm` (it's gitignored).
- [x] Clear not-found error naming `alzed-bridge` + setup instructions.

## Steps

```sh
# 1. Fork zed-industries/extensions to your account, then clone YOUR fork.
gh repo fork zed-industries/extensions --clone --remote
cd extensions

# 2. Branch.
git checkout -b add-al-extension

# 3. Add ALzed as a submodule (HTTPS, not SSH) at extensions/al.
git submodule add https://github.com/mhensberg2003/ALzed.git extensions/al
git -C extensions/al checkout main   # submodule must track a branch, not a detached commit

# 4. Add the entry to the top-level extensions.toml.
#    Because ALzed's manifest is in a subdirectory, the `path` field is REQUIRED.
#    Insert alphabetically (between any "ak..." and "alt..." entries):
#
#    [al]
#    submodule = "extensions/al"
#    path = "crates/extension"
#    version = "0.1.0"
#
#    (version MUST equal the version in crates/extension/extension.toml)

# 5. Keep ordering valid (CI enforces this).
pnpm install
pnpm sort-extensions

# 6. Commit and push to your fork.
git add .gitmodules extensions/al extensions.toml
git commit -m "Add AL extension"
git push -u origin add-al-extension

# 7. Open the PR against zed-industries/extensions:main
gh pr create --repo zed-industries/extensions \
  --title "Add AL extension" \
  --body-file ../ALzed/docs/zed-registry-pr-body.md
```

## After acceptance

- Bumping ALzed: tag a new version, then open a PR here updating the submodule
  pointer and the `version` in `extensions.toml` to match `extension.toml`.
- Users then install via Zed → Extensions → search "AL" (no dev-extension step),
  but still install `alzed-bridge` + the MS AL server separately (see README).
