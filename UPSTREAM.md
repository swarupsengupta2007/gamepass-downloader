# Importing upstream Xodus changes

How to bring new work from upstream Xodus (and the Xodus Nexus patch series) into this project without losing this project's changes.

The project root is the Xodus workspace. Upstream's tree sits directly in it (`Cargo.toml`, `crates/`, `.cargo/`), beside this project's `AGENTS.md`, `HANDOFF.md` and this file. Until 2026-10-31 it lived in a `xodus/` subfolder.

## What the workspace is made of

The workspace is three layers. Each update replaces the bottom two and replays the top one.

| Layer | Source | Current pin |
| --- | --- | --- |
| 1. Upstream | [xodus-gaming/xodus](https://github.com/xodus-gaming/xodus) | commit `53ea781dcd073e31d7ea68e894134e9c959bff04` (backend 0.2.12) |
| 2. Nexus patches | `assets/xodus-*.patch` in [xodus-nexus](https://github.com/domsvoboda/xodus-nexus), applied in the order of `assets/backend-source.lock.json` | xodus-nexus commit `768ba8c290e74f655cec9851fe8fb6f8d5f3fcc6`, patch set `linux-run-v46`, 45 patches |
| 3. This project | everything else in the root, including `AGENTS.md`, `HANDOFF.md` and `UPSTREAM.md` | the commits on `main` after the tag `nexus-linux-run-v46`: 27 new files, 26 changed upstream files, and the deletions listed below |

Checked on 2026-10-30: all 45 patches match their `sha256` in the lock file and apply cleanly with `git apply` to the pinned commit. Comparing that rebuilt baseline with the workspace gives exactly the layer-3 list below (plus the `license`, `sp-license` and `download --dry-run` removals made after that comparison).

Removed 2026-10-31 because the app does not use them: `crates/xodus-service/`, `README.md`, `assets/`, `docs/`, `scripts/`, `.github/`. They are part of layer 3 as deletions. A new `README.md` for this project took the upstream one's place the same day.

## The repository

Since 2026-10-31 the project is a git repository, public on GitHub at [swarupsengupta2007/gamepass-downloader](https://github.com/swarupsengupta2007/gamepass-downloader), built with the one-time setup below.

| Ref | What it is |
| --- | --- |
| `upstream-pin` (branch) | Layer 1: the pinned upstream commit with upstream's full history |
| `nexus-linux-run-v46` (tag) | Layer 2: one commit per Nexus patch on top of `upstream-pin`, the base layer 3 sits on |
| `main` (branch) | Layer 3: this project's commits on top of that tag |
| remote `origin` | the GitHub repository above |
| remote `upstream` | https://github.com/xodus-gaming/xodus |

Files are stored with LF line endings (`.gitattributes`), except `*.cmd`, which keeps CRLF. Before the setup, 72 files had CRLF line endings while upstream and the patches use LF. The setup normalized them.

A fresh clone needs the `upstream` remote again before an update: `git remote add upstream https://github.com/xodus-gaming/xodus.git`.

## Where updates come from

1. **A new Nexus release (preferred).** Nexus pins an upstream commit in `assets/backend-source.lock.json` and ships patches tested against it. Moving to a new lock moves layers 1 and 2 together, to a combination someone has already built.
2. **Upstream `main` directly.** Use this only for a fix you need before Nexus picks it up. You then carry the Nexus patches across yourself, and some will conflict.

Take a whole new pin, never single files. The crates depend on each other (`xodus`, `msixvc`, `msixvc-common`, `xodus-cli`).

The project folder keeps no copy of upstream or Nexus. Fetch them from GitHub into the project's repository or a scratch folder, and delete the scratch folder afterwards.

**Line endings on Windows.** Clone with `core.autocrlf false`. With `true` (Git for Windows' default, and this machine's global setting), the patches are checked out with CRLF, every one fails its `sha256`, and `git apply` fails against LF sources. The commands below set it per repository.

## One-time setup: give the project real history

Done on 2026-10-31. This is the record of how, and how to rebuild it if the repository is ever lost (start from a copy of the working tree without `.git`).

Build the history in a scratch folder, then move only `.git` into the project root. The working tree in the root stays the source of truth for layer 3.

```powershell
$repo  = 'D:\dev\gamepass-downloader'
$work  = Join-Path $env:TEMP 'xodus-history'
$nexus = Join-Path $env:TEMP 'xodus-nexus-pin'

# The Nexus release layer 3 sits on: lock file and patches.
git init $nexus
git -C $nexus config core.autocrlf false
git -C $nexus remote add origin https://github.com/domsvoboda/xodus-nexus.git
git -C $nexus fetch --depth 1 origin 768ba8c290e74f655cec9851fe8fb6f8d5f3fcc6
git -C $nexus checkout --detach FETCH_HEAD
$lock = Get-Content "$nexus\assets\backend-source.lock.json" | ConvertFrom-Json

# Layer 1: the pinned upstream commit, with its real history.
git init $work
git -C $work config core.autocrlf false
git -C $work remote add upstream https://github.com/xodus-gaming/xodus.git
git -C $work fetch upstream $lock.commit     # full history (about 4 MiB); a partial clone cannot be pushed to GitHub
git -C $work switch -c upstream-pin FETCH_HEAD   # not "upstream": that is the remote's name

# Layer 2: one commit per Nexus patch, in lock order, checksums enforced.
git -C $work switch -c nexus
foreach ($p in $lock.patches) {
    $file = "$nexus\assets\$($p.file)"
    if ((Get-FileHash $file -Algorithm SHA256).Hash.ToLower() -ne $p.sha256) { throw "checksum: $($p.file)" }
    git -C $work apply --index --whitespace=nowarn $file
    if (-not $?) { throw "does not apply: $($p.file)" }
    git -C $work commit -q -m "nexus $($lock.patchId): $($p.file)"
}
git -C $work tag "nexus-$($lock.patchId)"   # the base layer 3 sits on

# Layer 3 goes on its own branch.
git -C $work switch -c main
# %TEMP% is on C: and the project on D:, so Move-Item copies and then fails to delete
# git's read-only pack files. Copy, check, then delete the scratch folders.
Copy-Item -Recurse "$work\.git" "$repo\.git"
git -C $repo fsck
Remove-Item -Recurse -Force $work, $nexus
```

`git -C $repo status` now lists exactly this project's changes, plus the line-ending noise. Normalize the line endings and keep build output out of git:

```powershell
cd D:\dev\gamepass-downloader
Set-Content .gitattributes "* text=auto eol=lf`n*.cmd text eol=crlf`n*.png binary`n*.ico binary" -Encoding ascii   # cmd misreads labels in LF batch files
Add-Content .gitignore '/target'        # if not already ignored
git add --renormalize .
git add -A
git diff --cached --stat                # should match the file list below
```

Commit layer 3 in feature-sized commits rather than one, so a later replay conflicts in small, understandable pieces. A file that belongs to two groups can be split with `git add -p`. In the 2026-10-31 setup, `xodus-cli/src/lib.rs` and `commands.rs` went whole into "One exe and the window", so the earlier commits do not build on their own; only `main` as a whole does. The groups:

| Commit | Files |
| --- | --- |
| Windows credential store | `xodus/Cargo.toml`, `xodus/src/lib.rs`, `xodus/src/secrets.rs`, `xodus/src/tokens/backend/keychain.rs`, `xodus/src/windows_credentials.rs` |
| Sign-in on Windows | `xodus-cli/src/browser_auth.rs`, `xodus-cli/src/webview.rs`, `xodus-cli/src/commands/login.rs`, `xodus/src/api/xbox/auth.rs`, `xodus/src/api/xbox/mod.rs`, and the `run(…, username, …)` callers: `xodus-cli/src/package.rs`, `xodus-cli/src/commands/run.rs` |
| Xbox app hand-off | `xodus-cli/src/commands/adopt.rs`, `queue.rs`, `xodus-cli/src/xbox/{mod,sidecars,system}.rs`, `xodus-cli/Cargo.toml` (`windows-sys`) |
| Streaming speed and integrity | `xodus-cli/src/commands/streaming.rs`, `streaming_batch.rs`, `streaming_split.rs`, `streaming_progress.rs`, `streaming_resume.rs`, `streaming_preflight_tests.rs`; `msixvc/src/xvd.rs`, `streaming_http.rs`, `encrypted_image_tests.rs`, `msixvc/examples/*` |
| `verify` | `xodus-cli/src/commands/verify.rs` |
| No key or URL output | `xodus-cli/src/commands/license.rs` and `splicense.rs` (deleted), `xodus-cli/src/commands/download.rs` |
| Library and profile | `xodus-cli/src/commands/library.rs`, `profile.rs` |
| One exe and the window | `xodus-cli/src/lib.rs` (also the `License`, `SpLicense` and `Download` removals), `xodus-cli/src/commands.rs`, `xodus-cli/src/main.rs`, `crates/gamepass-gui/*`, `.cargo/config.toml`, `.cargo/msvc-static-crt.cmake`, `build.cmd` |
| Unused upstream parts | `crates/xodus-service/`, `README.md`, `assets/`, `docs/`, `scripts/`, `.github/` (deleted) |
| Project docs and repository settings | `AGENTS.md`, `HANDOFF.md`, `UPSTREAM.md`, `.gitattributes`, `.gitignore`; `README.md` came in a later commit |
| Lock file | `Cargo.lock` |

Before relying on this, rebuild and run the tests (see [Check the result](#check-the-result)). The working files did not change, so the result should match the current build.

## Updating to a new pin

### 1. Get the new sources

```powershell
$repo = 'D:\dev\gamepass-downloader'
$new  = Join-Path $env:TEMP 'xodus-nexus-new'
git clone -c core.autocrlf=false --depth 1 https://github.com/domsvoboda/xodus-nexus $new
$lock = Get-Content "$new\assets\backend-source.lock.json" | ConvertFrom-Json
$lock.commit; $lock.patchId; $lock.rust; $lock.patches.Count
```

Read the Nexus changelog and the new patch list before going on:

- Patches that are new or removed since the old lock.
- Upstream commits between the old pin and the new one: `git -C $repo fetch upstream $lock.commit`, then `git -C $repo log --oneline 53ea781..FETCH_HEAD`.
- The Rust version in `rust`. Install it if it is newer than yours.

### 2. Build the new base (layers 1 and 2)

Commit or stash any work first. Switching branches swaps the files in the project root, including `AGENTS.md`, `HANDOFF.md` and this file, which are not on the base branch (read this file from `main` in another window, or copy it out first). `target/` is ignored and stays.

```powershell
cd D:\dev\gamepass-downloader
$old = 'nexus-linux-run-v46'               # the tag main currently sits on
git fetch --filter=blob:none upstream $lock.commit
git switch -c "base-$($lock.patchId)" FETCH_HEAD
foreach ($p in $lock.patches) {
    $file = "$new\assets\$($p.file)"
    if ((Get-FileHash $file -Algorithm SHA256).Hash.ToLower() -ne $p.sha256) { throw "checksum: $($p.file)" }
    git apply --index --whitespace=nowarn $file
    if (-not $?) { throw "does not apply: $($p.file)" }
    git commit -q -m "nexus $($lock.patchId): $($p.file)"
}
git tag "nexus-$($lock.patchId)"
```

If a patch does not apply, stop. The lock and the patches disagree, so wait for a fixed Nexus release.

**Upstream `main` without a Nexus release:** instead of the loop above, replay the old patch commits onto the new upstream commit:

```powershell
git switch -c base-next $old
git rebase --onto FETCH_HEAD upstream-pin base-next
git tag nexus-next-<date>                  # stands in for "nexus-<patchId>" below
```

Resolve the conflicts inside the Nexus patches yourself. A patch that conflicts and is Linux-only (Wine, gamescope, memfd, D-Bus, KDE wallet, keyring unlock or session) can be dropped with `git rebase --skip`. Note each one you drop in `HANDOFF.md`.

### 3. Replay this project's commits

```powershell
git switch main
git switch -c update-$($lock.patchId)      # keep main as the rollback point
git rebase --onto "nexus-$($lock.patchId)" $old
```

During a rebase, `--ours` is the new base and `--theirs` is this project's commit, the reverse of a merge. Resolve each conflict with the [hot spots](#hot-spots) below, then `git add` and `git rebase --continue`.

`Cargo.lock` conflicts: take the new base's file, then let Cargo add this project's dependencies back:

```powershell
git checkout --ours Cargo.lock
cargo build --release -p gamepass-gui
git add Cargo.lock
```

Do not run a blanket `cargo update`. It moves every dependency at once, and then a failure cannot be traced to the upstream change. The Nexus `cargoLockSha256` will not match this project's lock file, and that is expected.

### 4. Check the result

Check it as described in [Check the result](#check-the-result) below. When everything passes, fast-forward `main` to the update branch, and keep the old tag for rollback:

```powershell
git switch main
git merge --ff-only update-$($lock.patchId)
git branch -f upstream-pin $lock.commit    # the next update starts from here
```

Next time, `$old` is `nexus-<this patchId>`.

Then update the records:

- The pins (upstream commit and xodus-nexus commit, `git -C $new rev-parse HEAD`) in `AGENTS.md` (the table under "Reference sources"), in this file (the layer table and the one-time setup), and in `HANDOFF.md`.
- Delete `$new`.
- A dated note in `HANDOFF.md`: the new pin and patch id, patches dropped or rewritten, conflicts resolved, test results.

## Hot spots

These are the places where upstream changes are most likely to collide with this project's changes, and what to keep.

| Area | This project's change | When upstream touches it |
| --- | --- | --- |
| `xodus-cli/src/main.rs` | Moved into `xodus-cli/src/lib.rs` (`pub fn run(args)`); `main.rs` is three lines. | Upstream edits to `main.rs` (new subcommands, new setup) never apply cleanly. Port them by hand into `lib.rs` and keep `main.rs` as the wrapper. Keep `run` parsing with `display_name` from `argv[0]`. |
| `xodus/src/api/xbox/auth.rs` | `authenticate_xbox_user` tries the RPS ticket as issued, then as `t=`, then with no prefix, each only after a 400 (`ticket_forms`). `refusal_diagnostic` prints status and `x-err`. | If upstream fixes the 2026-10-28 `d=` break its own way, keep whichever form succeeds on a live sign-in. Keep the fallback order and the diagnostics. |
| `xodus/src/api/xbox/mod.rs` `run(…)` | Takes the signed-in `username` (upstream sent the literal `"USERNAME"`). Also prints `NEXUS_TICKET_DIAGNOSTIC_V1` on a fault. | New upstream callers of `run` need the username: `tokens.get_user()?.username`. |
| `msixvc/src/xvd.rs`, `streaming_http.rs` | Every page is checked against the hash tree before it is decrypted or written; a mismatch is refetched from another CDN root (`set_mirrors`, `mirror`), up to 4 tries, then `transfer-integrity`. `PageCheck` for `verify`. | Any upstream rewrite of `download_segment_http` must keep the page check. **Never write a page that fails its hash.** Rerun `streaming_damaged_page_is_fetched_again_and_never_written`. |
| `xodus-cli/src/commands/streaming*.rs` | Ranked CDN roots, small-file batches (`streaming_batch.rs`), 16 MiB chunks with idle-connection takeover (`streaming_split.rs`), `package_urls` and `content_key` helpers, `.xodus-streaming-progress.json`, `.xodus-streaming-complete.json` (required by `adopt`). | Keep the completion marker and progress file, because the window and `adopt` read them. Keep the journal format compatible, or a resumed download starts again from zero. |
| `xodus/src/secrets.rs`, `tokens/backend/keychain.rs`, `windows_credentials.rs` | Windows Credential Manager (service `Xodus Service`), DPAPI file for entries over 2560 bytes. | Keep it on Windows. Do not ship the sample file keyring. |
| `xodus-cli/src/webview.rs`, `browser_auth.rs` | Windows signs in through Edge or Chrome over a DevTools pipe (`--do-not-de-elevate`), not WebView2. | Keep the `cfg(windows)` route to `browser::run`. Upstream WebView changes only affect other platforms. |
| `xodus-cli/src/lib.rs` `SubCommand`, `commands.rs`, `commands/download.rs` | Upstream's `license` and `sp-license` commands and `download --dry-run` are removed (they hand out license keys and CDN URLs). | Upstream changes to them come back as conflicts or as a re-added `commands/license.rs` or `commands/splicense.rs`. Drop them again. |
| `xodus-cli/src/commands/profile.rs` | Also reads `legalCountry` from `accounts.xboxlive.com` and outputs only `country`. | Keep that field; the window uses it for the market. |
| `xodus-cli/Cargo.toml`, `xodus/Cargo.toml` | `windows-sys`, `windows-native-keyring-store`. | Merge feature lists rather than replacing them. |
| `.cargo/config.toml` | `+crt-static` for `x86_64-pc-windows-msvc`, and a CMake toolchain file for the static C runtime. | Keep both. If upstream adds `rustflags` for `x86_64`, add them to the Windows table too. The Windows table already repeats the AES flags, so they apply however Cargo combines the two tables. |
| `crates/xodus-service/`, `README.md`, `assets/`, `docs/`, `scripts/`, `.github/` | Deleted; nothing in the build uses them. | Upstream edits come back as modify/delete conflicts. Keep them deleted (`git rm`). If upstream moves code the app needs into one of them, raise it with the project owner. `README.md` is now this project's own: on a conflict keep ours (during a rebase, `git checkout --theirs README.md`). |
| `crates/gamepass-gui/` | Not upstream. | Breaks only when `xodus-cli` command names, flags or JSON output change. Check `cli.rs` against `--help` and the JSON of `queue`, `account`, `profile`, `resolve-package`, `streaming --progress-json` and `adopt --json --log`. |

Upstream also ships commands this project leaves alone: `run` (Linux/Wine), cloud saves, achievements. Take their changes as they come; they need to build, not work, on Windows.

## Check the result

From the project root:

```powershell
.\build.cmd all     # cargo build --release --locked -p gamepass-gui -p xodus-cli
.\build.cmd test    # cargo test --release --workspace --no-fail-fast
```

These tests already failed on this machine before any update and can be ignored:

- `msixvc` `streaming_http_retries_are_bounded_and_keep_actual_status` (closed-port behaviour on Windows).
- `xodus-cli` `session ticket_real_transport_timeout_and_http_refusal_classification`.
- `xodus-cli` `package_plan premium_http…`, which fails some runs.

Any other failure is new and must be explained before the update is kept.

**The exe is still self-contained.** It should import only Windows system DLLs (no `VCRUNTIME140`, no `api-ms-win-crt-*`):

```powershell
& "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\<version>\bin\Hostx64\x64\dumpbin.exe" /dependents target\release\gamepass-downloader.exe
```

If a new C dependency built with CMake shows up as `/MD` (link warnings `LNK4098`/`LNK4217`), run `cargo clean --release -p <that -sys crate>` and build again so it picks up `.cargo/msvc-static-crt.cmake`.

**Smoke test with the signed-in account.** Use a pipeline so PowerShell waits for the windowed exe:

```powershell
$exe = 'target\release\gamepass-downloader.exe'
& $exe account --json | Out-String
& $exe queue --json | Out-String
& $exe profile --json | Out-String          # gamertag, plan, country
& $exe verify "D:\Xbox\<an installed game>\Content" --json | Out-String
```

**End to end.** Before calling the update done, run the user's workflow once on a small game: queue in the Xbox app and pause, Download in the window, Send to Xbox app, Resume installation in the Xbox app, and the game launches.

## Rules an update must not break

From `AGENTS.md`. Check the diff of every update against them:

- No mode that skips licensing.
- Never place or decrypt `KEEP_ENCRYPTED_ON_DISK` executables; only Gaming Services writes them.
- Never write a page that fails its hash.
- Don't dump content keys or share package/CDN URLs; CDN roots are printed by index only.
- No `wdapp register`; leave UWP/`WindowsApps` titles out.
- Keep the GPLv3 `LICENSE` and the copyright notices from upstream.

Upstream ships `license` (writes content keys to `.cik` files for XvdTool), `sp-license` (prints the RSA key held in a license block) and `download --dry-run` (prints CDN URLs). This project removed all three on 2026-10-30: the `License` and `SpLicense` variants and their arms in `xodus-cli/src/lib.rs`, `commands/license.rs` and `commands/splicense.rs`, and the `dry_run` flag in `commands/download.rs`. When an update touches those places, keep them removed. If an update adds or widens commands like these, raise it with the project owner rather than taking it silently.

## Rolling back

Nothing above rewrites `main` until the final fast-forward. To give up an update:

```powershell
git rebase --abort      # if a rebase is still running
git switch main
git branch -D update-<patchId>
```

The `nexus-<patchId>` tags mark every base that has been used. The `base-<patchId>` branches can be deleted once their tag exists.
