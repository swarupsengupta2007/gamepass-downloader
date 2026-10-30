# Handoff — continue on Windows

Written 2026-10-25 from a Linux session. Updated 2026-10-26 on this Windows machine. Stages 1–4 are done. Xbox app adoption worked by hand for Ori (see "Xbox adoption: the sequence that worked"). The open work is automating that sequence.

Read `AGENTS.md` first. This file is the session context that `AGENTS.md` does not repeat: what was checked out, what was decided, and the first commands to run.

## Goal

A Windows CLI that, with the user's own Microsoft account:

1. Signs in and stores tokens in Windows Credential Manager.
2. Lists product ids the account can install (purchase or Game Pass).
3. Downloads the Windows desktop game files into a folder.
4. Places those files where the official Xbox app installs that game, so Play uses the local copy and does not download the game again.

The Xbox app stays the launcher and keeps the entitlement check. This tool does not launch games, ship Wine/Proton, or skip licensing. The slow official download is the reason the tool exists, so step 4 is part of the product, not a side idea. It waits until a plain `streaming` install has written one real game.

## What already exists

This folder is not a git repository. The only project files are `AGENTS.md` and this document.

The sources were first read from reference clones in `external/`. That folder was removed on 2026-10-31; the pins are:

| Repository | Pin | Notes |
| --- | --- | --- |
| https://github.com/domsvoboda/xodus-nexus | `768ba8c290e74f655cec9851fe8fb6f8d5f3fcc6` | Launcher 0.3.12. MIT. Electron. It does not download bytes. |
| https://github.com/xodus-gaming/xodus | `53ea781dcd073e31d7ea68e894134e9c959bff04` | Commit pinned by `assets/backend-source.lock.json` in xodus-nexus for backend 0.2.12. GPLv3. |

Paths below such as `src/main/index.ts` or `scripts/BACKEND.md` are in xodus-nexus at that pin. To get the files, clone into a scratch folder as `UPSTREAM.md` shows. On Windows, set `core.autocrlf false` in that clone, or every patch fails its checksum.

## Decisions already made

- Windows is the first platform. Linux Secret Service and the Electron sign-in host are out of scope until a Windows install works.
- The product is `xodus-cli`, built on Windows from the pinned commit plus the Nexus patch series. The published Nexus backend tarball is Linux x86_64 only. `scripts/build-backend.mjs` refuses to run unless `process.platform` is Linux, so it cannot be the Windows build.
- Build `xodus-cli` with default features. That keeps `native-webview` (Tao/Wry). Nexus passes `--no-default-features` to strip the webview and talk to Electron. Do not copy that flag.
- On Windows, Wry uses WebView2. `login` opens the Microsoft page in that window. If a passkey stalls, open the same login page in the system browser and let the CLI finish the token exchange.
- Token storage stays on `keyring_core::Entry` in `crates/xodus/src/secrets.rs`, service name `Xodus Service`. Replace the non-Linux sample file store with `windows-native-keyring-store` (Windows Credential Manager). A generic credential blob is limited to 2560 bytes. A token that does not fit is protected with DPAPI and written in the user profile. Do not ship the sample store.
- Copying the Rust download code into this repo makes the tool GPLv3. Keep the Xodus copyright and license notices.
- Downloads require a real entitlement. No mode that skips licensing, dumps content keys, or prints CDN URLs for other tools.

## How a download works

Nexus install ends in `src/main/index.ts` (`prepareInstall`, `runDownload`). It spawns:

```text
xodus-cli streaming <product-or-content-id> <destination> --market XX --parallel N
  --selected-content-id <uuid> --progress-json --control-stdin --download-limit-kib <n>
```

The Rust path, before Nexus patches, is:

1. `crates/xodus-cli/src/package.rs` `get_content_id` reads `https://displaycatalog.mp.microsoft.com/v7.0/products/{productId}` and picks the `Windows.Desktop` content id. A bundle can prompt on a TTY. Nexus replaces that with `resolve-package --json`.
2. `get_packages` sends an XSTS token for `http://update.xboxlive.com` to `GET https://packagespc.xboxlive.com/GetBasePackage/{contentId}` and joins the first CDN root with the `.msixvc` relative URL.
3. `crates/xodus-cli/src/commands/streaming.rs` range-reads the XVD header (`msixvc::streaming::HttpRead`), lists files, then calls `get_license`.
4. `crates/xodus-cli/src/license.rs` posts to `https://licensing.mp.microsoft.com/v7.0/licenses/content`. No entitlement is a hard failure. Store device limit is licensing code 501.
5. `msixvc` `download_file_http` range-reads each changed file and writes the game directory. Default parallelism is 4. The container prefix is kept as `.xodus-streaming.msixvc` so a later run skips unchanged files.

`xodus-cli download` saves raw package files after an interactive prompt. Do not use it.

Account product ids come from a Nexus command, not from upstream Xodus:

```text
xodus-cli library --json --market XX
```

It calls `https://beige.xboxservices.com/PCGAFD/entitlements` and returns rows with `productId`, `status`, `isTrial`, `sharingSource`, `endDate`, `satisfiedByProductId`, and `special`. It does not label a row owned or Game Pass. That classification is TypeScript in xodus-nexus `src/main/account-library.ts`:

- Keep rows that are `Active`, not a trial, `sharingSource` `None`, and not expired.
- Walk `satisfiedByProductId`. If a parent is one of `CFQ7TTC0KGQ8`, `CFQ7TTC0KHS0`, `CFQ7TTC0P85B`, `CFQ7TTC0K5DJ`, `CFQ7TTC0K6L8`, `CFQ7TTC10QFD`, the game is Game Pass.
- A row with no parent is owned.
- Drop the subscription products themselves, trials, shared copies, and rows with an unknown special-license flag.

Then `resolve-package` keeps only `Windows.Desktop` / `MSIXVC` packages whose kind is `Game`, `GameAddOn`, or `Durable`. `--market` is the two-letter Store market for the account. It is not always `US`.

Patch behavior is documented in xodus-nexus `scripts/BACKEND.md`. The ordered patch list and checksums are `assets/backend-source.lock.json` there (backend 0.2.12, Rust `1.99.0`). Upstream `Cargo.toml` allows Rust 1.98.0. The Nexus build checks that `rustc` is the locked 1.99.0. Use 1.99.0.

## Windows machine setup

- Windows 10 or 11, 64-bit.
- WebView2 Runtime. Present on Windows 11 and on updated Windows 10. Install the Evergreen runtime if `login` cannot open a window.
- Visual Studio Build Tools, Desktop development with C++, MSVC, and the Windows SDK. Wry and Credential Manager need the MSVC toolchain, not the GNU toolchain.
- `rustup` with the MSVC target. Toolchain `1.99.0`.
- Git.
- `protoc` on `PATH`. The `xodus` crate runs `prost-build` even when `xodus-service` is not built.
- A Microsoft account that owns a PC game or has Game Pass, and a Store market code.

OpenSSL is not a starting requirement. `reqwest` on Windows uses the system TLS stack. Add it only if a crate fails the build asking for it.

Correction, 2026-10-31: OpenSSL is required. `openssl-sys` comes in through `eappx` → `xts-mode` (the `extract-eappx` command). This machine has Win64 OpenSSL 4.1 at `C:\Program Files\OpenSSL-Win64`, with `OPENSSL_DIR`, `OPENSSL_INCLUDE_DIR` and `OPENSSL_LIB_DIR` (`lib\VC\x64\MD`) set, which is why it never failed here. The first release run (a clean Windows runner) failed on it. The local exe imports no OpenSSL DLL even though it links the `MD` import libraries, because the linker drops the unused calls. The release workflow links the static `MT` libraries instead (`OPENSSL_LIBS=libssl_static:libcrypto_static`, `OPENSSL_STATIC=1`), so the exe stays self-contained either way, and it fails if an OpenSSL DLL shows up in the imports.

## First build

Work in a copy of the Xodus tree. Apply every patch in `backend-source.lock.json` in order with `git apply`. Later patches depend on earlier ones. Do not hand-pick a subset until a patch fails because it is Linux-only.

```powershell
cargo build --locked -p xodus-cli
```

No `--no-default-features`. Expect compile failures from Linux-only patches (`nix`, memfd, D-Bus, KDE wallet, gamescope). Fix those with `cfg(target_os = "linux")` or by skipping that patch when it cannot apply. Keep `login`, `library`, `resolve-package`, and `streaming` building.

Then change `crates/xodus/src/secrets.rs` so Windows uses Credential Manager, with the DPAPI fallback above 2560 bytes.

Leave out `xodus-cli run`, `xodus-service`, cloud saves, achievements, and the Electron app.

## Prove it on this machine

These three cannot be verified from Linux:

1. `xodus-cli login` opens WebView2, completes Microsoft sign-in, and a second process still sees the account (`xodus-cli` account or `library`).
2. `xodus-cli library --json --market XX` prints product ids for that account. A small wrapper, or a new flag, marks each id owned or Game Pass using the rules above.
3. `xodus-cli streaming <productId> <destination> --market XX` writes a game directory containing `MicrosoftGame.config` for one entitled PC game.

Device identity on Windows reads SMBIOS in `crates/xodus/src/hardware.rs` through `smbios-lib`. If `login` fails before the browser opens, start there.

## Official Xbox app

`streaming` writes a loose folder: `MicrosoftGame.config` at the destination root, game files beside it, and `.xodus-streaming.msixvc` in that same folder. On this PC the Xbox library root is `D:\Xbox`, not `C:\XboxGames` or `D:\XboxGames`. `C:\XboxGames` exists and only contains `GameSave`. A finished Windows desktop game lives at `D:\Xbox\<Display Name>\Content\`, with registration files beside `Content`. An install that has not finished uses the content-id GUID as the folder name. The app has no import-folder command. Dropping the loose folder somewhere else does not make the library show the game as installed. The 2026-10-26 trial is recorded in the next section.

`wdapp register` is the wrong tool. It is a GDK developer command, it requires Developer Mode, and it does not register a Store install in the retail Xbox library.

What to prove, by hand, on one entitled game after `streaming` works:

1. Put the extracted game files in the `Content` directory the Xbox app uses for that title. Keep `.xodus-streaming.msixvc` out of that directory.
2. In the Xbox app, install the game onto that drive, or use Manage, Files, Verify and repair if it already lists the game.
3. Success is the app showing the game as installed and Play starting without a full download.

If that cold copy still downloads the game, use the sequence that works for people moving an existing official install: let the app create the folder and its registration, pause once that skeleton exists, replace the partial `Content` tree with the extracted files, and resume. Verify and repair then hashes what is already on disk.

Do not automate either sequence until one of them skips the download on this machine. Some titles lock the executable while Gaming Services is writing it, so a decrypted extract may not match an official tree. Record which sequence worked, and which files the app still replaced. Leave UWP titles that install under `WindowsApps` out of this step.

## Where the Windows build stands (2026-10-26)

`xodus/` is a full tree of commit `53ea781dcd073e31d7ea68e894134e9c959bff04` plus the Nexus patch series, built with Rust 1.99.0 and `cargo build --locked -p xodus-cli`. Default features stay on. `external/` was not edited.

- `login` uses the system Chrome window. WebView2 only offered a password, and this account is passwordless (passkey, then Microsoft Authenticator after the passkey prompt is declined).
- Tokens persist through Windows Credential Manager, service name `Xodus Service`, with DPAPI when an encoded secret exceeds 2560 bytes. A new process still sees the account.
- `library --json --market IN` lists the account. For en-IN that was 82 owned and 723 Game Pass. `resolve-package` keeps Windows desktop `MSIXVC` only.
- `streaming` wrote Ori and the Will of the Wisps. Product id `9N8CD0XZKLP4`, market `IN`, Windows.Desktop content id `c4984363-e73f-404e-83da-5e26e0de4a6e`. The console XVC content id `acc17b37-134d-446a-9385-bac4d75ae55a` is not a PC package. Package identity is `Microsoft.Patagonia`, family `Microsoft.Patagonia_8wekyb3d8bbwe`, version `1.0.8978.0`. Store title is "Ori and the Will of the Wisps". `MicrosoftGame.config` DefaultDisplayName is "Ori And The Will Of The Wisps".
- Journal durability was the speed fix. Game bytes flush at the end of each file and every 32 MB inside a large file. The shared `.nexus-streaming-v1.jsonl` calls `sync_all` when 64 new checkpoint records are waiting, or when a checkpoint offset is at least 32 MB, or when the download loop ends. A crash can lose the last small-file journal lines. Resume re-hashes that tail. `--parallel 16` rewrote the same game to `D:\XboxDownloads\Ori-retest` in about 6 minutes. 4362 game files matched the first extract. The only difference was `.xodus-streaming.msixvc`: the shorter file is an exact prefix of the longer one.

Keep both extracts. The pristine copy is `D:\XboxDownloads\Ori and the Will of the Wisps`. The retest is `D:\XboxDownloads\Ori-retest`. Do not put a game inside the project tree. Do not start a second `streaming` process against the same folder.

## Xbox adoption: the sequence that worked (Ori, 2026-10-26 late)

The Xbox app now lists Ori as "Just installed!" with Play, and the install queue is empty. Nothing was downloaded after the residency edit. The steps below come after the earlier trial further down, which had already put the extracted files into `Content` while the install was paused.

The resume decision lives in two places, and neither of them is `.xvs`:

- `HKLM\SOFTWARE\Microsoft\GamingServices\StreamingCheckpoints`, value `{30FF2225-…}#{<content id>}`. JSON queue item with `State` (`Running`, `Failed`), `Status`, and progress. This is what the shell shows. `.xvs` is a copy the service rewrites from it.
- `<id>.xvi` (`crdi-xvc`), the per-chunk residency table. Resume streams every chunk this table marks missing. It never hashes `Content`.

`.xvi` layout, worked out from Ori (in progress) and Pentiment (finished):

| Offset | Meaning |
| --- | --- |
| `0x240` | content id GUID, then FILETIME, `u32 3`, `u32 7` |
| `0x350` | `u32` segment (file) count, the same as `.smd` `segment_count` |
| `0x360` | `u64` streamed bytes for this plan |
| `0x368` | `u64` `FFFF…` until finalized, then the total. Set by the service. |
| `0x374` | `u16` 6 while streaming, 7 when finished, then an unknown `u16`. Set by the service. |
| `0x378` | `u64` resident package bytes. Equals queue `StreamedBytes`. |
| `0x380` | plan id GUID; `0x390` `u32` entry count, the same as the `.xsp` record count at `0x27C` |
| `0x3B0` | flags. `0x3F` while streaming, `0x7F` when finished. Set by the service. |
| `0x4F2` | bitmap, zero in every file seen so far |
| roughly `0x4F2 + segments / 4` | one 4-byte entry per `.xsp` record: `02 00 00 01` new data resident, `03 00 00 01` copy data done, `00 00 00 00` missing. In-flight entries carry other first bytes and more data (`88 …`, `C8 …`, `BA 19 00 00`, `19 02 00 00`, `0F 3D 01 00`). The start is **not** a function of the segment count: `0x4F2 + ceil((segments + 2) / 4)` happened to fit Pentiment, Ori and Resonance, but across 17 installs the gap is 1 byte short of to 13 bytes past `segments / 4` (NINJA GAIDEN 4: 3768 segments, table at `0x8A2`, formula says `0x8A1`). `sidecars.rs` finds it from the data: the recorded Content path starts right after the table, and before the path exists the table is the first non-zero byte (record 0, the package header, is always streamed first). |
| after the table | UTF-16 NT path `\??\D:\Xbox\<folder>\Content`, NUL-terminated. Written as soon as the install is queued, before `Content` exists. |

`.xsp` records start at `0x1000`. Each is 16 bytes: source, flag (`0` new data, `0x88000000` copy), target block, block count (4 KiB blocks). `total_download` is at `0x2C8` and `disk_space_required` at `0x2D0`.

The header cache file named `<content id>` beside `Content` is byte-identical to the first 93,712,384 bytes of `.xodus-streaming.msixvc`.

What was done:

1. Backups of every sidecar plus `reg export` of `StreamingCheckpoints`, `StreamingTracking`, and `PackageRepository`.
2. Elevated script (`mark-resident.ps1`, one UAC prompt). It stops `GamingServices` and `GamingServicesNet`, waits until both processes are gone, opens `.xvi` exclusively, sets every entry to `02 00 00 01`, sets `0x360` and `0x378` to the `.xsp` total, and starts both services. `0x368`, `0x374`, and `0x3B0` were left for the service to set. The script refuses to write unless the entry count matches `.xsp`, the path string follows the table, and every record is new data.
3. After the service started, the checkpoint `State` was `Failed` and the service did not retry by itself. In the Install queue, the item's `…` menu, **Resume installation** (not Restart install).
4. Within about 20 seconds the checkpoint was gone. Gaming Services registered `Microsoft.Patagonia_1.0.8978.0_x64__8wekyb3d8bbwe` (`Get-AppxPackage` Status Ok). It wrote `PackageRepository\Package` and `Root` entries, created the junction `D:\WindowsApps\<PFN>` → `D:\Xbox\Ori And The Will Of The Wisps\Content\`, renamed the GUID folder to the display name, and wrote `.xvs` `Completed`, `Result` 0, every `StreamedBytes` equal to `12251607040`, `LaunchRegionsResident`. `XvddType` in `.xvs` still said 6 right after finalization.

SHA256 compare afterwards, extract against `Content`: 4360 of 4362 files were identical. `appxmanifest.xml` differs, and `gamelaunchhelper.exe` and `SmallLogo.ico` were added. Gaming Services writes those at finalization (Pentiment's `Content` has `gamelaunchhelper.exe` too). `oriandthewillofthewisps-pc_Data\sharedassets17.assets.resS` was the chunk in flight (`C8`) when `.xvi` was edited. It kept the service's `BUILTIN\Users:(N)` deny after finalization, so the game could not have read it. `icacls <file> /remove:d "BUILTIN\Users"` worked from the normal, non-elevated shell, and the file then hashed identical to the extract. Any automation has to pause the queue item before the edit, so no chunk is in flight, and check for leftover deny ACEs afterwards.

### Automated: `queue`, `adopt`, and the GUI (2026-10-27)

- `xodus-cli queue [--json]` lists queue items with the install folder, the queued build, and whether the item can be adopted (or why not). It also lists installed content ids. No keyring or sign-in is needed.
- `xodus-cli adopt <content id> <download folder> [--copy] [--json] [--log FILE]` does the hand-off above. It refuses without elevation. `streaming` now writes `.xodus-streaming-complete.json` (content id, build id, version) only after a successful run, and deletes it when a run starts. `adopt` requires that marker.
- Header cache length is the end of the leading run of `RESIDENT` XVC regions (header, embedded XVD, mutable data, `XVC-MD`, `FS-MD`). For Ori that is 93,712,384 bytes.
- `.smd` is not in the package's user data for Ori. Gaming Services gets it some other way. `adopt` checks the download against `.smd` when one exists and does not create one.
- A freshly paused install (Resonance: A Plague Tale Legacy, `9PPBNCXP0096`, content id `7b0ecd90-9694-4ce7-8e49-7454781d3cce`, build `142295b1-…`) had State `Paused` and Operation `Stopped`. It had the header cache preallocated (690,733,056 bytes) with 25.7 MB streamed, `.xsp`, `.xvi` (flags `0x07`, path already recorded), `.xct`, `.xvs`, and an empty `Shaders` folder. There was no `Content` folder and no `.smd`. The `.xsp` `total_download` (29.7 GB) is smaller than `disk_space_required` (82.2 GB).
- `streaming` probes every CDN root (foreground and background lists) with a 4 MiB range read from the middle of the package and uses the fastest. It prints `CDN root N: X MB/s` to stderr, by index only. Before this it always took the first root. One slow edge reads as 0 B/s, which looks the same as the Xbox app's own stall. A direct `curl` test from this PC got 22–29 MB/s from every host in the item's `.xvs` paths, including the edge the stalled run was connected to.
- `streaming --progress-json` also mirrors its latest record, with `updatedAt` in Unix milliseconds, into `.xodus-streaming-progress.json` in the destination. It writes atomically about once a second, immediately on phase changes, and keeps writing during quiet phases. The GUI uses it to show downloads started by another process, and treats a file older than 15 seconds as stale.
- Small neighbouring files download in batches, and every page is checked against the hash tree; see "Small-file batching, page hashes" below.
- Large files download in parts (`commands/streaming_split.rs`): a file with at least 256 MiB left is fetched over up to 8 connections, each with its own file handle. Since 2026-10-28 the parts are 16 MiB chunks taken in order as connections free up; before that they were 8 fixed ranges of at least 128 MiB. Each part is a `SegmentFile` slice, so XTS decryption (tweak from the page's index in the section) works unchanged. The journal still holds one offset per file, so only the gap-free prefix is checkpointed; a killed run refetches parts after it. Pause works inside each part. Before this, each file was one HTTP stream. On 2026-10-27 the CDN gave each connection 0.5–1 MB/s at times, which put Resonance's 37.6 GB `DATAS\COMMON.DPC` at about 13 hours and NINJA GAIDEN 4's 22.8 GB `Assets\@image0.dat` at about 8. With parts, both downloads ran at 12–14 MB/s.
- Resume is slow to start. The prefix keeps its `.xodus-streaming-tmp.msixvc` name until a run finishes and is truncated on each start, so a resumed run fetches the whole package header again (690 MB for Resonance, a few minutes over one connection). It then re-verifies each file's checkpoint. Progress restarts at 0, and files already on disk are credited only as they are reached. The GUI smooths speed over 10 seconds for that reason. Use the release build: debug-build hashing made resume verification take several minutes.
- `gamepass-gui` reads `queue --json` every 3 seconds and resolves titles with `resolve-package`. It downloads to `<install drive>:\XboxDownloads\<CONTENT ID>` with `streaming --progress-json --control-stdin`, and runs `adopt` through `ShellExecuteEx` `runas`, following its `--log`.

### Resonance: freshly paused install adopted from the GUI (2026-10-27)

This is the end-to-end proof of the user's workflow: queue in the Xbox app, pause at 25.7 MB, download in `gamepass-downloader`, Send to Xbox app, Resume in the Xbox app.

- `streaming` downloaded 81,462,164,913 bytes to `D:\XboxDownloads\7B0ECD90-…` (one interrupted run, then resumed) and wrote the completion marker.
- First `adopt` (copy mode) copied all 45,651 files, then failed on opening the header cache with `ERROR_ACCESS_DENIED`. The paused service had left its `BUILTIN\Users:(N)` write lock on that file, which also blocks an elevated administrator, down to reading the ACL. `adopt` now runs `takeown /A` and `icacls /remove:d *S-1-5-32-545` on access denied. It treats only `ERROR_SHARING_VIOLATION` (32) as "service is back". Copy mode now logs placed files too, so a re-run skips them.
- Second `adopt`: every file was already in place. It wrote the header cache (690,733,056 bytes) and marked every `.xvi` entry resident.
- Resume in the Xbox app: within 5 seconds the checkpoint showed `State Running`, `Operation Completed`, StreamedBytes 82,249,052,160. It disappeared about 15 seconds later. Gaming Services registered `FocusHomeInteractiveSA.APlagueTaleFelons_1.3.0.0_x64__4hny5m903y3g0` (Status Ok), added it to `PackageRepository\Package`, and renamed the folder to `D:\Xbox\Resonance- A Plague Tale Legacy`. It never created `.smd` and did not need it.
- Compare afterwards: 45,648 files the same size as the download. The three largest (37.6, 10.9 and 6.2 GB) were SHA-256 identical. `appxmanifest.xml` was rewritten, `gamelaunchhelper.exe` and `Resources\WinStore\SmallLogo.ico` were added (same as Ori), and no deny ACEs were left.
- Copy mode on the same SATA SSD dropped from about 200 MB/s to about 36 MB/s after about 68 GB, once the drive's write cache filled. Move mode (the default) is a rename.
- Play failed: Windows said the program is a 16-bit application. See below.

### Encrypted executables: why Play failed, and the fix (2026-10-27)

- `Resonance.exe` in the download did not start with `MZ`; every DLL did. The package flags it `KEEP_ENCRYPTED_ON_DISK` in `SegmentMetadata.bin` (it is the only flagged file in Resonance). Ori's package has no segment metadata (NTFS layout only), and upstream treats every `.exe` there as kept encrypted; its three `.exe` files were ciphertext too.
- A normal Xbox install keeps those files encrypted on disk as well, but each carries an NTFS extended attribute `$KERNEL.GAMING.ENCRYPTED` (about 4 KB; it starts `97 FB 92 1C 02 00 …` and holds the on-disk length) plus `$KERNEL.PURGE.GAMING.UNMODIFIED`. The `gameflt` minifilter decrypts at launch and denies even administrators a plain read (Pentiment's exe: access denied). `$KERNEL.` attributes can only be set from kernel mode, so a copied file can never be made launchable from user mode.
- Fix in `adopt`: read the flagged files from the downloaded package header (`xbox::service_written_files`, falling back to NTFS metadata and `.exe`), leave them out of the placement, delete any copy already in `Content`, and leave every `.xsp` record that overlaps their pages missing in `.xvi` (byte counts reduced to match). Resume then streams only those chunks (Resonance: record 11, pages 171139–187027, 65 MB) and Gaming Services writes the files itself with the attribute.
- `.xsp` record layout confirmed: `+8` `u32` first XVD page, `+12` `u32` page count. Record 11's start page times 4096 equals `Resonance.exe`'s offset from segment metadata.
- Verify and repair in the Xbox app creates a queue item of `Type` `Repair` with a fresh `.xvi`, and re-streams the whole game. `queue` now lists repairs, and a paused repair is adopted like a fresh install. That is how an install adopted before this fix (Ori) gets its executables rewritten.
- Proven on Ori (2026-10-27, 04:2x): Verify and repair, paused at 0 B; fresh download in the GUI; `adopt` placed 4,359 files and left 3 records (3.1 MB) missing; Resume in the Xbox app finished in about a minute. All three `.exe` files then carried `$KERNEL.GAMING.ENCRYPTED` and `$KERNEL.PURGE.GAMING.UNMODIFIED`, a plain read of them was denied (as for Pentiment), and Play started `oriandthewillofthewisps-pc.exe` with its game window (2 GB working set).
- Full fresh runs from the GUI, 2026-10-27 (queue in the Xbox app, pause, download in `gamepass-downloader`, Send to Xbox app, Resume):
  - NINJA GAIDEN 4 (`9P0SQ1S72G4J`, `Microsoft.TOROretail_1.0.43646.0`): paused at 313 MB; 3,767 files moved; 1 record (47.5 MB) left for `NINJAGAIDEN4-WinGDK.exe`; Resume finished in about 30 seconds; folder renamed to `D:\Xbox\NINJAGAIDEN4`; exe tagged; game window opened and ran.
  - Resonance (uninstalled, download folder deleted, reinstalled): paused at 17.8 MB; 45,650 files moved; 1 record (65.1 MB) left for `Resonance.exe`; Resume finished in about 4 minutes; exe tagged; the game reached its title screen.
  - Ori (via Verify and repair, above).
- Download tail: thousands of tiny files (Resonance's 3,874 `cache\*.psocache`, 9 KB each) took several minutes at about 80 KB/s, because each was its own HTTP request. Fixed 2026-10-28, see below. The 690 MB package header is still fetched over one connection before any file starts, which took 10+ minutes while other downloads shared the line.
- `service_written_files` runs the package parser on its own thread and runtime. The NTFS parser bridges sync reads with `block_in_place` + `SyncIoBridge`, which deadlocked inside a nested `Handle::block_on` (the first GUI hand-off of Ori hung at "Checking").

### Small-file batching, page hashes, damaged CDN bytes (2026-10-28)

- Batching (`commands/streaming_batch.rs`): files under 1 MiB not yet complete in the journal are grouped in package order into runs spanning at most 16 MiB, with gaps of at most 256 KiB (files already downloaded). Each run is one range request into memory (fetched as raw pages), then each member is decrypted with `extract_file` from that buffer and written whole, then checkpointed. A file alone in its run downloads as before. `file://` sources are not batched. Checked on Ori: 3,436 small files (517.5 MB) re-fetched in under a minute, byte-identical to the one-request-per-file path.
- Page hashes: each section's `data_hashs` (from the hash tree) is SHA-256 of the encrypted page as served, first 20 bytes (`examples/xvd_page_hashes.rs` matched all 515 pages of Ori's two kept-encrypted exes). `download_segment_http` now checks every page before decrypting or writing it. On a mismatch it refetches from that page from the next CDN root (`streaming_http::set_mirrors`, roots ranked fastest first by the probe), up to 4 times per page, then fails with `transfer-integrity`. Each refetch logs `NEXUS_TRANSFER_DIAGNOSTIC_V1 transfer-integrity-retry 0`. The `file://` path (`extract_file`) is not checked.
- Why: the Ori install adopted on 2026-10-27 has 4 damaged files (`sharedassets10/101/117.assets`, `StreamingAssets\Audio\GeneratedSoundBanks\Windows\483843194.wem`). Each has wrong bytes from file offset 3296 to about 5360–6800, where the right copy has zeros and Unity table data; both older Ori extracts and fresh verified downloads agree with each other. An unverified test run on 2026-10-28 reproduced the same wrong bytes for `483843194.wem` and damaged a different `.wem`, so a CDN location was serving bad bytes for some ranges, not random transit noise. Later runs forced onto each of the 4 roots all served correct bytes. The fallback to another root is covered by a unit test only.
- `xodus-cli verify <folder> [--header FILE] [--repair] [--json]` (`commands/verify.rs`): reads the package header from the folder's `.xodus-streaming.msixvc`, or for an installed `Content` folder from the Xbox app's header cache beside it (the file named after the content id), gets the content key from the account's license, and checks every page of every listed file on up to 8 threads (`msixvc` `PageCheck`: plain pages are XTS-encrypted again and hashed). The last partial page of a file is checked zero-filled first; if only that page fails, it is fetched (hash-checked) and the file's bytes compared. Files without hashes (the plain region: logos, `MicrosoftGame.config`), the Xbox app's encrypted executables (plain read denied) and, in an installed folder, `appxmanifest.xml` are reported as not checked. `--repair` fetches only the damaged pages of a plain file and writes them in place, and whole files when missing or the wrong size (through a `.xodus-repair` temp file), then checks them again. It never writes a kept-encrypted file into an installed folder. About 250–300 MB/s here.
- Results, 2026-10-28: Ori had 10 damaged files (the 4 above plus `level199`, `level245`, `sharedassets104/115/166/17.assets.resS`, all differing from both older extracts), repaired; Ori then reached its title screen. NINJA GAIDEN 4 had 5 (single pages in `Assets\@image0.dat`, `@patch_image1.dat`, two `Movies\*.msd`, one `.wem`), repaired page by page. Resonance and RESIDENT EVIL 2 were clean. Backups of Ori's first 4 damaged files are in the session scratchpad only. The test folder `D:\XboxDownloads\batch-check` has been deleted.
- Big files now download in 16 MiB chunks that up to 8 connections take in order (`streaming_split.rs`), instead of 8 fixed parts. The fixed parts left a download waiting on the slowest connection's whole part at the end. When no chunk is left to start, an idle connection takes over the unfinished chunk with the most left: it fetches the whole chunk again from its start (the other connection may be dropped mid-write, and both write the same verified bytes), the first to finish wins and the other is dropped. A takeover that fails leaves the chunk to the original connection. Unit test: a chunk whose first answer stalls for 60 s finishes in seconds. A real run with chunks only (no takeover yet), 2026-10-28 on a poor line (best root 8.7 MB/s): RE2 22.14 GB in 23.5 minutes, last 30 MB about 90 seconds. With takeover, same line quality (best root 9.1 MB/s, one root dead): 22.14 GB in 21.5 minutes, the last 180 MB in under 30 seconds; `verify` on the result: all 13 files match. Whole-line slowdowns (every connection at a few MB/s for minutes) remain and are outside this code.
- Xbox sign-in broke on 2026-10-28 around 02:00 local time, for stored and fresh logins alike: the live.com SOAP exchange (`MBI_SSL`, site `user.auth.xboxlive.com`) now returns the compact ticket as `d=…`, and `user.auth.xboxlive.com/user/authenticate` answers 400 with `x-err: 00000005` and an empty body. The same ticket sent as `t=…` is accepted. The prefix is undocumented (community clients have needed `d=`, `t=` or none at different times). `authenticate_xbox_user` sends the ticket as issued, then after a 400 as `t=`, then without a prefix (`ticket_forms`), and logs each refusal. Refusals from user auth and XSTS print `NEXUS_SESSION_DIAGNOSTIC_V1 <stage>-refused <status> <x-err>`; before, any HTTP error read as "Xbox authentication request failed. Check your connection".
- `login` crashed as soon as the Chrome window opened: started from an elevated process, current Chrome relaunches itself without administrator rights and the first process exits, so the relaunched window has no DevTools pipe ("Sign-in window closed."; `devtools_pipe_reaches_a_blank_page` failed for this reason). Fixed with `--do-not-de-elevate`. A browser with an update waiting (`new_chrome.exe` / `new_msedge.exe` beside it) also relaunches itself on start, so one without a waiting update is preferred, and the error names the cause.
- RESIDENT EVIL 2 (`BWMH951M3G3P`, content id `43507237-dbbe-4a21-8eec-75b384af60f4`, `F024294D.GAMEResidentEvil2biohazard2_1.2.0.0`), end to end from the GUI on 2026-10-28: paused at 0 B; 22.14 GB in about 18.5 minutes at up to 36 MB/s (22 files, one 22.6 GB `re_chunk_000.pak`, so batching barely applied); Resume installed in 30 seconds; `re2.exe` tagged; the game ran (window titled RESIDENT EVIL 2, 1.6 GB). The download still stalled at 0 B/s for 1–1.5 minutes several times, and its last gigabyte took about 3 minutes while the big file's parts finished unevenly.

### One exe (2026-10-29)

- `xodus-cli` is now a library crate with a thin `main.rs`; `xodus_cli::run(args)` parses and runs a command on its own Tokio runtime. `gamepass-gui` links it: `gamepass-downloader.exe` with no arguments opens the window, and with arguments it is the CLI (`gamepass-downloader verify <folder>`, `queue --json`, …; help and `--version` name the exe that was run). The window runs its own exe for every job, as it ran `xodus-cli.exe` before, so a download stays a separate process (stop by killing it, a crash cannot take the window down), and `adopt` is still one `runas` launch. `xodus-cli.exe` is still built by `-p xodus-cli` for development, but nothing needs it.
- `login` no longer gets a console window (the exe is a windows-subsystem program). Its stderr goes to `%TEMP%\gamepass-downloader\login.log` rather than a pipe, because the sign-in browser can inherit a pipe and keep it open; a failure is shown as the window's notice.
- From a terminal, the exe attaches to the terminal's console when it has no stdout/stderr. PowerShell does not wait for a windows-subsystem exe or capture its output unless it is in a pipeline: use `gamepass-downloader verify X | Out-Host` (or `| Out-String`, `| ConvertFrom-Json`); then `$LASTEXITCODE` is set too. `$x = & exe …` alone runs detached and captures nothing.
- Static C runtime: `.cargo/config.toml` adds `+crt-static` for `x86_64-pc-windows-msvc`, and points CMake builds at `.cargo/msvc-static-crt.cmake` (`CMAKE_MSVC_RUNTIME_LIBRARY`), because CMake ignores the `-MT` the `cmake` crate passes (zlib-ng, pulled in by `eappx`, came out `/MD` and the link warned LNK4098/LNK4217). cmake-built crates do not rerun on that variable: after changing it, `cargo clean --release -p libz-ng-sys`. The release `gamepass-downloader.exe` (about 24 MB) imports only Windows system DLLs, no VCRUNTIME140 or UCRT. WebView2 is not loaded: on Windows sign-in uses Edge or Chrome.
- Checked: a lone copy of the exe in an empty folder showed the account and the queue, and `verify` of the installed Ori matched 4,352 files (10 unchecked: hashless files and the encrypted exes). Tests as before (known failures: `streaming_http_retries_are_bounded_and_keep_actual_status`, `ticket_real_transport_timeout_and_http_refusal_classification`; `package_plan premium_http…` passed this time). A download and an `adopt` have not been run through the one exe yet; their code paths changed only in which exe is started.

- Account in the window (2026-10-29): the header shows the account as a pill (gamerpic, gamertag, best Game Pass plan; green ring while a plan is active) from `profile --json`, read after `account --json`. Clicking it opens a panel with the gamerpic, gamertag, email, plan badge, Refresh profile and Sign out (asks first; disabled while a download or hand-off from this window runs; runs `logout`, which keeps the device identity). Signed out, a sign-in card sits above the queue; Sign in runs `login` and can be cancelled (`taskkill /T` on the `login` process, which also closes the browser it opened). Plan names come from `xodus::models::xbox::subscriptions`, preferring Ultimate, Premium, PC Game Pass, Essential, console. Store market is a drop-down of country names (`src/markets.rs`, two-letter codes shown beside them; a code not listed, from the Windows region, is kept as is). It starts at the Windows region and switches to the account's country once the profile loads, until the user picks one: `profile --json` now also reads `accounts.xboxlive.com/users/current/profile` (same XSTS token, contract version 2) and outputs only `country` from its `legalCountry` (two letters, uppercased; the rest of that record, which includes the email and date of birth, is dropped). Window settings (`src/settings.rs`) live in `%APPDATA%\gamepass-downloader\settings.json`: `{"account": <signed-in username>, "market": <picked code or null>, "parallel": n, "keepDownload": bool}`, written through a temp file and rename once a value changes (not while the pointer is down, so a slider drag is one write). They belong to the account: nothing is saved while signed out; at start they are applied only when the stored account is the one signed in; signing out (or starting signed out) deletes the file; and when an account signs in without saved settings the market drops any pick made while signed out and follows the account's country, since downloads use the account. A "Use account's country" link beside the list clears a pick. Checked with the account signed in: an old file without `account` was ignored (market back to India) and rewritten with it; Japan and keep-folder survived a restart; the link went back to India. The sign-out deletion was not exercised (it needs a real sign-out). It returned `IN` for the development account; a refusal logs `NEXUS_SESSION_DIAGNOSTIC_V1 account-country-refused <status>` and leaves it out. Sign-out and sign-in from the window were confirmed working by the user on 2026-10-29. The icon (green tile, arrow into a tray) is drawn in code in `src/icon.rs`: the window uses it at 64 px, and `build.rs` writes it as a multi-size `.ico` (16–256 px, 32-bit BMP entries) and embeds it as icon resource 1 with `embed-resource` (needs `rc.exe` from the Windows SDK), so Explorer and shortcuts show it. Checked on screen: header pill, panel, sign-out confirmation (cancelled), and the signed-out card (in a throwaway build that forced that view). A real sign-out followed by a sign-in from the window has not been run (it needs the user's passkey).

- Removed 2026-10-30, at the owner's request: upstream's `license` command (wrote content keys to `.cik` files for XvdTool; `commands/license.rs` deleted) and `download --dry-run` (printed CDN URLs). `download` still saves the encrypted package files without showing URLs. `sp-license` (printed the RSA key held in an SPLicenseBlock passed in; `commands/splicense.rs` deleted) was removed the same day. `clep` is still there. `UPSTREAM.md` lists the removal as a hot spot for upstream updates.

- Project folder cleaned 2026-10-31, at the owner's request, after a full copy to `D:\dev\gamepass-downloader-backup-2026-10-31` (everything except `xodus\target`, plus the two release exes). Removed from `xodus/` because nothing in the build uses them: the `xodus-service` crate, upstream's `README.md`, `assets/` (Xodus branding images), `docs/` (upstream wiki), `scripts/` and `.github/` (upstream CI), and the `target\debug` build cache (15 GB). The same day `external/` was removed too: the `UPSTREAM.md` steps now clone xodus-nexus at `768ba8c` from GitHub into a scratch folder. Checked first: that clone's lock file and all 45 patches are byte-identical to the ones in `external/`, and every patch matches its `sha256`, but only with `core.autocrlf false` (this machine's global setting is `true`, which turns the patches into CRLF and fails every checksum). Then the contents of `xodus/` were moved up into the project root, which is now the Cargo workspace, and `xodus/` is gone. Sections above that say `xodus/` mean the root now; the exe is `target\release\gamepass-downloader.exe`. The moved `target/` was reused as is (a `--locked` release build finished in a second). `build.cmd` was added as the build helper. Then the project became a git repository with the `UPSTREAM.md` one-time setup (full upstream history, 45 Nexus patch commits, this project's commits on `main`) and was pushed to a public GitHub repository, `swarupsengupta2007/gamepass-downloader`. Commits use a repository-local `user.name` and the GitHub no-reply `user.email`, so the public history carries no personal address; git on this machine has no global identity. `Cargo.lock` was refreshed by a release build and `--locked` builds again. `cargo test --release --workspace` fails only the three known tests listed in `UPSTREAM.md`.

To automate this for a new title: let the app create the install folder and queue item, pause it, stop Gaming Services, write the extracted files into `Content`, write the header cache from the msixvc prefix, mark `.xvi` resident, start the services, and press Resume installation. That has only been proven on Ori, and only after the earlier copy steps below. Pressing Resume from code (rather than in the app UI) has not been done.

## Xbox adoption trial (Ori, 2026-10-26)

Superseded by the section above. This was the state before the `.xvi` edit: the Xbox `Content` tree was the downloaded game, and the app showed a failed partial download with no Play.

### Layout the app created

Pressing Install onto `D:\Xbox` created `D:\Xbox\C4984363-E73F-404E-83DA-5E26E0DE4A6E`. The app ignored a folder that had already been created under the display name. The GUID folder has not been renamed. Finished titles on this PC (Pentiment, Palworld, SIFU) use the display name.

`Content` holds the loose game files: 4362 files, 12148901887 bytes. Beside `Content`, not inside it:

| File | Role |
| --- | --- |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E` (93712384 bytes) | Package header cache. This is the ~98 MB gap between the loose extract and the package `TotalBytes` of 12251607040. |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E.8D270559-F932-481F-8FBF-06250BFAA1EC.xsp` | Streaming manifest. Build id and plan id are `8d270559-f932-481f-8fbf-06250bfaa1ec`. Sku id on the in-progress record was `0010`. |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E.smd` | Segment metadata. |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E.xct` | 4096-byte table. It already had the same `07 00 00 00`, `FF`, and `7F` pattern as a finished game, including while the install was still in progress. Flipping those bytes is not the completion switch. |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E.xvi` | Residency index (`crdi-xvc`). Not modified. |
| `C4984363-E73F-404E-83DA-5E26E0DE4A6E.xvs` | UTF-16 LE JSON, no BOM. This is the record the shell reads. |

A finished Pentiment `.xvs` has `Operation` `Completed`, `Result` `0`, `RetriedErrorCount` `0`, `LaunchRegionsResident` true, `XvddType` `7`, and `StreamedBytes` equal to `TotalBytes` on the Package, Install, Launch, and Network pairs. Ori's failed record had `Operation` `Failed`, `Result` `-2015363057` (the dialog's `0x87e0000f`), `LaunchRegionsStreaming` true, and `XvddType` `6`. `RetriedErrorCount` climbed from 2 to 73 while the service kept retrying. Instance id prefix seen on these records: `{30FF2225-7FE8-4CD9-AFFB-FB48EA541B1F}#`.

Compare game files only. Leave the header cache and the five sidecars out of that compare.

### Sequences that did not make the app show the game installed

- A cold copy into `D:\Xbox\Ori and the Will of the Wisps` before the app had registered the title. After a restart the library still did not list it as installed. Install then created the GUID folder and started its own download. `wdapp register` was not used.
- Pause, replace `Content`, resume. Resume continues the chunk stream and overwrites files. It does not hash what is already in `Content`.
- Closing the Xbox window. Gaming Services keeps writing. The file it is writing gets `BUILTIN\Users:(N)`. Once that process is gone, `icacls /remove:d BUILTIN\Users` clears the deny. `takeown` needs an elevated token. The normal shell is not elevated.
- `Stop-Service GamingServices` works without elevation, and the queued install starts the Automatic service again within seconds, including after the Xbox app processes are killed. `sc.exe config` to `demand` or `disabled` returns `OpenService FAILED 5` even from an elevated token. Do not plan on changing the start type.
- Writing `.xvs` while `gamingservices` or `gamingservicesnet` still has the file open. The service can report Stopped or StopPending while the process is alive, and an elevated write then fails with access denied. The first approved elevated script copied files first, the service came back during that copy, and the failed write was swallowed. Verify still showed Failed, streaming, `XvddType` 6.
- A second elevated run wrote `.xvs` before the service restarted, and that Completed record survived the service start. Its recopy was a no-op: `Start-Process robocopy` returned exit 16 because the source path contains spaces and was passed as one argument. Call `robocopy` directly. Exit codes 0–7 are success. Exit 9 means some files failed. Exit 16 is a bad command line. `/MIR` applies to `Content` only, and the exclude list is `.xodus*` and `.nexus*`.
- A third elevated run, after the processes were confirmed gone, set `Operation` to `Completed`, `Result` to `0`, `RetriedErrorCount` to `0`, `LaunchRegionsResident`, `XvddType` `7`, and every `StreamedBytes` equal to its `TotalBytes` (`12251607040`). That text was still on disk after Gaming Services started and after the Xbox app was killed and relaunched. The queue did not follow it. It still said Error, 5.4 GB of 11.4 GB, 47%, "Unable to download", dialog "Something went wrong", error `0x87e0000f`, button "Restart install". 47% is the streamed amount from before the edit, so the shell is not using the numbers written into `.xvs`. `.xvi` was left unchanged. The folder was left under the content id.

Do not click Restart install. That starts an official download over the copied files. The error dialog was closed with its X.

### What did land in Content

Uninstall removed the previous Ori folder. The product page stayed on Uninstalling until the Xbox app was killed and relaunched, then showed Install. Install onto `D:\Xbox` created the GUID folder. Gaming Services was stopped once that folder existed (143 files, about 302 MB). `robocopy /MIR /MT:16` from `D:\XboxDownloads\Ori and the Will of the Wisps` into `Content`, excluding `.xodus*` and `.nexus*`, finished with exit 1: 4362 files copied, 0 failed, 11.314 GB.

A later full byte compare of those 4362 files against the pristine extract found one unreadable file, `oriandthewillofthewisps-pc_Data\sharedassets17.assets.resS` (188826720 bytes, `Users:(N)`). An earlier pass had the same kind of read error on `sharedassets1.assets.resS`. After the deny was removed, both files compared identical. Nothing in `Content` differed from the download. The content-id cache beside `Content` is separate and was not part of the compare.

The copy has to happen while Gaming Services is stopped. A recopy started after the service had come back failed on `resources.assets.resS` with access denied, and that file had to be copied again with the service stopped.

### Still open

The app has not shown the game as installed and has not offered Play. Verify and repair (Manage, Files) is the hash path and has not been used on this install. Resume and Restart install both continue a chunk download.

The likely remaining sources of the 47% error are the `.xvi` residency index, the `.xsp` plan, or a transfer record the service holds outside `.xvs`. That is a hypothesis. The next edit of a sidecar still needs an elevated process, and only after the Gaming Services processes are actually gone. Tell the user what the UAC prompt is for. UAC is on the secure desktop, so it cannot be clicked from automation. Restore both services to Running if an attempt leaves them stopped.

Do not automate this sequence until one trial makes the app show the game installed and a new compare shows which files, if any, the app replaced. Do not delete either extract under `D:\XboxDownloads`.

## Out of scope for the first Windows session

- Linux Secret Service, KDE wallet, and the `xodus-keyring-*` unlock/create flow.
- Electron `--external-webview-stdio`.
- Pause/resume and the speed limit, until a plain `streaming` install works. The patches for `--progress-json`, `--control-stdin`, and `--download-limit-kib` can stay applied.
- Reimplementing the MSIXVC container. Port the Rust code.
- Automating the Xbox app adoption. The first session only has to get `streaming` to write a game folder. The adoption proof above is the next session, and it is required before the tool is done.
