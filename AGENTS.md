# Game Pass downloader

Standalone tool that downloads Microsoft Store / Xbox Game Pass PC game files for an account that already owns or subscribes to them, then places those files where the official Xbox app installs that game so Play uses the local copy. The Xbox app stays the launcher and keeps the entitlement check. This tool does not launch games, ship Wine/Proton, or bypass licensing.

The reason it exists: the Xbox app's own download keeps stalling at 0 B/s. The user's workflow is:

1. Queue the game in the Xbox app and pause it.
2. Open this tool. It lists the paused installs from the Xbox app queue.
3. Download the game in this tool.
4. The tool moves the files into the Xbox app's install folder and hands the install back.
5. Resume installation in the Xbox app finishes without downloading, and the game is playable from the Xbox app.

Windows is the first platform. Linux Secret Service, KDE wallet, and the Electron sign-in host wait until a Windows install works.

## Reference sources

The project folder holds no copies of these. Read them on GitHub at the pins below, or clone them into a scratch folder (see `UPSTREAM.md`). Do not treat them as the product.

| Repository | Pin | What it is |
| --- | --- | --- |
| [Xodus Nexus](https://github.com/domsvoboda/xodus-nexus) | `768ba8c290e74f655cec9851fe8fb6f8d5f3fcc6` (launcher 0.3.12) | Electron/TypeScript launcher. MIT. Its `assets/backend-source.lock.json` pins the upstream commit and lists the `assets/xodus-*.patch` series with checksums. |
| Upstream [Xodus](https://github.com/xodus-gaming/xodus) | `53ea781dcd073e31d7ea68e894134e9c959bff04` (backend 0.2.12) | The commit pinned by that lock file. GPLv3. |

Nexus does not download bytes itself. Install calls the separate `xodus-cli` binary.

## What to extract

The product is the authenticated **streaming install** path:

1. Sign in (device identity + Microsoft account tokens).
2. Resolve a Store product id to a Windows desktop `MSIXVC` content id.
3. Ask Xbox for that package's CDN URL.
4. Ask Microsoft licensing for a content key. Stop if the account is not entitled.
5. Range-read the container and write decrypted game files into a destination folder.

Upstream command: `xodus-cli streaming <product-or-content-id> <destination> --market XX --parallel N`.

Nexus wraps that in `src/main/index.ts` (`prepareInstall` / `runDownload`). The bytes, license, and container format live in the Rust crates, then in the Nexus patches (`assets/xodus-*.patch` in xodus-nexus).

Leave these out of the downloader: Wine/Proton runtime, `xodus-cli run`, cloud saves, achievements, Steam shortcuts, and the Electron UI.

The older `xodus-cli download` command saves raw package files after an interactive prompt. It does not produce an extracted game folder. Do not use it as the install path.

## Pipeline

Public catalog, no account:

- `https://displaycatalog.mp.microsoft.com/v7.0/products/{productId}`
- Pick the `Windows.Desktop` package and its content id.
- Upstream `get_content_id` (`crates/xodus-cli/src/package.rs`) can prompt on a TTY when a product is a bundle. Nexus replaces that with non-interactive `resolve-package --json`.

Authenticated package URL:

- Device and user tokens become an XSTS token for `http://update.xboxlive.com`.
- `GET https://packagespc.xboxlive.com/GetBasePackage/{contentId}`
- Join the first CDN root with the `.msixvc` relative URL.

License, then extract:

- `POST https://licensing.mp.microsoft.com/v7.0/licenses/content`
- A missing entitlement is a hard failure (not owned, or not in the current Game Pass tier). Microsoft Store also enforces a device limit (Nexus surfaces licensing code 501).
- `streaming` (`crates/xodus-cli/src/commands/streaming.rs`) parses the XVD header over HTTP, lists files from package metadata and NTFS segment metadata, then downloads each changed file with an HTTP Range request (`msixvc` `download_file_http`). Default parallelism is 4.
- This project's `streaming` also splits large files into parallel ranges, fetches runs of small neighbouring files with one request each, and checks every page against the package hash tree (SHA-256 of the encrypted page, first 20 bytes) before writing it, refetching a damaged page from another CDN root. A CDN location has served damaged bytes; never write a page that fails its hash. `xodus-cli verify <folder> [--repair]` checks an installed game or a download against the same hashes and refetches only damaged pages; it never writes the Xbox app's encrypted executables.
- Output is a normal game directory (often with `MicrosoftGame.config`). The container prefix is kept as `.xodus-streaming.msixvc` so a later run can skip unchanged files.
- Nexus adds pause/resume, a `.nexus-streaming-v1.jsonl` checkpoint journal, `--progress-json`, `--control-stdin`, and `--download-limit-kib`. See `scripts/BACKEND.md` in xodus-nexus.

## Platform and license constraints

- Published Nexus backends are Linux x86_64 tarballs. They are not a Windows download. The Windows tool is a build of `xodus-cli` from the pinned commit plus the install-path Nexus patches.
- Sign-in on Windows uses the upstream Tao/Wry window. Wry uses WebView2 there. Do not port the Electron `--external-webview-stdio` host for the first version. If WebView2 stalls on a passkey, open the same login page in the system browser and let the CLI finish the token exchange.
- Token storage goes through `keyring_core::Entry` (`crates/xodus/src/secrets.rs`). On Windows, replace the sample file store with Windows Credential Manager (`windows-native-keyring-store`, service name `Xodus Service`). A generic credential blob is capped at 2560 bytes. When a token does not fit, protect that entry with DPAPI and store it in the user profile. Do not ship the sample persisted keyring.
- Device provisioning on Windows reads SMBIOS (`crates/xodus/src/hardware.rs`, `smbios-lib`). That path has to be checked on a real Windows machine. This Linux checkout cannot verify WebView2, Credential Manager, or SMBIOS.
- The launcher is MIT. The backend is GPLv3. Copying the Rust download code into this repo makes the downloader GPLv3 and requires keeping copyright and license notices.
- Downloads require the user's own Microsoft account and a real entitlement. Do not add a mode that skips licensing, dumps content keys for other tools, or shares package URLs.

## Windows build order

1. Windows `xodus-cli` with `login`, `logout`, `library --json`, `resolve-package --json`, and `streaming`. Include the Nexus patches those commands need. Leave out `run`, Wine, and `xodus-service`.
2. Credential Manager (DPAPI when a token exceeds 2560 bytes) so `login` survives a new process.
3. `library` lists entitled product ids and marks each owned or Game Pass. `resolve-package` keeps only Windows desktop `MSIXVC` games.
4. `streaming` installs a chosen product id into a destination folder, with progress and resume.
5. `queue` and `adopt` hand a finished download to a paused Xbox app install. See below.
6. The `gamepass-gui` crate (binary `gamepass-downloader.exe`, egui) is the user-facing window. It links `xodus-cli` as a library, so the one exe is also the CLI when given a command (`gamepass-downloader verify <folder>`). The window runs its own exe with a command for every job (queue, account, title, login, download, elevated `adopt`) and holds no download or format logic of its own.

Prefer porting the Rust CLI over reimplementing the container format. Do not vendor the Electron app.

The project root is the Cargo workspace (pinned commit plus Nexus patches plus this project's changes) and a git repository, public on GitHub at `swarupsengupta2007/gamepass-downloader`; `UPSTREAM.md` describes its branches. Pushing a `v<version>` tag that matches `crates/gamepass-gui/Cargo.toml` runs `.github/workflows/release.yml`, which builds the exe on a Windows runner and publishes it as a GitHub release. `build.cmd` (or `cargo build --release --locked -p gamepass-gui`) builds `target/release/gamepass-downloader.exe`, a single self-contained exe: the C runtime is linked statically (`.cargo/config.toml`, plus `.cargo/msvc-static-crt.cmake` for CMake-built C libraries), so it imports only Windows system DLLs. It needs no other file beside it. `-p xodus-cli` (`build all`) still builds a standalone `xodus-cli.exe` for development. `build test` runs the workspace tests. `build.cmd` checks for `cargo`, `protoc` and `cmake` first, and moves a running `gamepass-downloader.exe` aside to `.exe.old` so the link step can replace it. To take new upstream Xodus or Nexus patch releases, follow `UPSTREAM.md`.

## Official Xbox app

The download exists so the Xbox app can play the game without fetching those bytes again. The app remains the thing that launches the game and checks the entitlement.

The Xbox app has no import-folder command, and a cold copy of the files is ignored. `wdapp register` is a GDK developer registration, needs Developer Mode, and does not make the retail library treat the folder as the Store install. Do not use it. Leave UWP titles that install under `WindowsApps` out of this.

Library folders are listed in `<drive>:\.GamingRoot` (`RGBX`, a `u32` count, then NUL-terminated UTF-16 folder names). The user picks the folder in the Xbox app (on the development machine it is `D:\Xbox`). Never hardcode it. An install that has not finished lives in `<root>\<CONTENT ID>\`. Gaming Services renames it to the display name when it finishes.

How Gaming Services decides what to download (worked out on this machine; see `HANDOFF.md` for the byte layouts):

- The queue item is the JSON value `{library id}#{CONTENT ID}` under `HKLM\SOFTWARE\Microsoft\GamingServices\StreamingCheckpoints` (`State` `Paused`/`Running`/`Failed`, `Status.Operation`, progress, build id). `.xvs` beside the install is a copy the service rewrites from it. Editing `.xvs` does nothing.
- `<CONTENT ID>.xvi` is the residency index: one entry per `.xsp` streaming-plan record. Resume downloads every chunk this index marks missing. It never hashes `Content`, so files placed there are overwritten by the stream unless the index says they are present.
- The header cache file named `<CONTENT ID>` is the package's leading resident regions (XVD header, metadata, filesystem metadata), byte for byte.
- Finished installs appear under `PackageRepository\Package` (package full name → instance id), with an AppX registration and a `D:\WindowsApps\<PFN>` junction to `Content`. Gaming Services creates all of that itself during finalization.

The hand-off (`xodus-cli adopt <content id> <download folder>`, elevated):

1. Refuse unless `streaming` left its completion marker, the download's XVD `pduid` equals the queued build id and the `.xsp` file name, the queue item is not streaming, and the `.xvi` layout checks pass.
2. Clear leftover `BUILTIN\Users` deny ACEs in `Content` (Gaming Services sets them on files it is writing), move the downloaded files into `Content` (`--copy` keeps the download), and reset their ACLs to inherit from `Content`. Files the package keeps encrypted on disk (the game executables) are not placed; see below.
3. Stop Gaming Services, open `.xvi` and the header cache exclusively, write the header cache from `.xodus-streaming.msixvc`, delete any copy of the encrypted files from `Content`, mark every `.xvi` entry resident except the records covering the encrypted files, and start the services again.
4. The user presses Resume installation in the Xbox app. Gaming Services downloads the few chunks left missing (the executables, tens of MB), then finalizes: registration, junction, folder rename, `.xvs` `Completed`.

Encrypted executables: files flagged `KEEP_ENCRYPTED_ON_DISK` in the package's segment metadata (or every `.exe` in an NTFS-only package such as Ori) stay encrypted on disk in a real install too. Gaming Services tags each with a `$KERNEL.GAMING.ENCRYPTED` extended attribute, which the `gameflt` driver needs to decrypt the file at launch. User mode cannot set `$KERNEL.` attributes, so a copied executable fails with "16-bit application". Only Gaming Services may write these files. Never place them, and never try to decrypt them for the Xbox app.

Do not compute the `.xvi` entry table offset from the segment count. Find it from the data (`sidecars.rs` `locate_table`).

Verify and repair in the Xbox app queues a `Repair` item that re-streams the whole game. A paused repair can be adopted like a fresh install.

Proven by hand on Ori (an install that had already streamed about half the game), and end to end from the GUI on Resonance: A Plague Tale Legacy (queued and paused at 25.7 MB: no `Content`, no `.smd`, `.xvi` flags `0x07`). In both cases Resume finalized in seconds without downloading, but the games did not launch because the encrypted executables had been copied (fixed as above). After the fix, Ori (through Verify and repair), NINJA GAIDEN 4 and a fresh reinstall of Resonance were each run end to end from the GUI; Resume downloaded only the executables (3–65 MB) and all three games launched from the Xbox app. RESIDENT EVIL 2 followed on 2026-10-28 the same way. Gaming Services rewrote only `appxmanifest.xml` and added `gamelaunchhelper.exe` and `SmallLogo.ico`. A paused install can leave a `BUILTIN\Users` deny ACE on the header cache. `adopt` takes ownership and removes it.
