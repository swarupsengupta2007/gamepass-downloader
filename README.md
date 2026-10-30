# gamepass-downloader

A Windows tool that downloads a Microsoft Store / Xbox Game Pass PC game for your own account, then hands the files to the official Xbox app to finish the install.

It exists because the Xbox app's own download can stall at 0 B/s for hours. The Xbox app stays the launcher and still checks your entitlement. This tool doesn't launch games and doesn't bypass licensing. It can only download games your account owns or that your current Game Pass plan includes.

## How it works

1. In the Xbox app, start installing the game, then pause it.
2. Open `gamepass-downloader.exe`. It lists the paused installs from the Xbox app's queue.
3. Click **Download**. The game is downloaded and decrypted into `<drive>:\XboxDownloads\<content id>` on the same drive as the install. Every page is checked against the package's hash tree before it is written. A damaged page is fetched again from another CDN server.
4. Click **Send to Xbox app** and accept the administrator prompt. The tool briefly stops Gaming Services and moves the files into the paused install. It then records them as already downloaded and starts the services again.
5. In the Xbox app, press **Resume installation**. The app downloads only what it must write itself (the game's encrypted executables, usually a few MB to tens of MB), then finishes the install. Play from the Xbox app as usual.

A game's executables stay encrypted on disk in a real install, and only Gaming Services can write them in a form the game can launch from. The tool therefore never places or decrypts those files and leaves them to the Xbox app.

This has been run end to end on Ori and the Will of the Wisps, NINJA GAIDEN 4, Resonance: A Plague Tale Legacy and RESIDENT EVIL 2. A paused **Verify and repair** in the Xbox app can be completed the same way.

## Requirements

- Windows 10 or 11, 64-bit, with the Xbox app and Gaming Services installed.
- A Microsoft account that owns the game, or has a Game Pass plan that includes it. Microsoft Store device limits still apply.
- Microsoft Edge or Google Chrome for sign-in.

Download `gamepass-downloader.exe` from [Releases](https://github.com/swarupsengupta2007/gamepass-downloader/releases). It is a single file with nothing to install, and `SHA256SUMS.txt` beside it has its checksum.

Only Windows desktop games that install through the Xbox app into an Xbox library folder (`MSIXVC` packages) are supported. Older UWP titles that install under `WindowsApps` are not.

## Using the window

- **Sign in with Microsoft** opens a browser window for the normal Microsoft sign-in, including passkeys. Your tokens are stored in Windows Credential Manager. Tokens too large for a credential entry are stored as DPAPI-protected files in your profile.
- **Store market** defaults to your account's country.
- **Parallel downloads** sets how many connections a download uses.
- **Keep the download folder** copies the files into the Xbox app's folder instead of moving them.
- A download can be paused, and a stopped download resumes where it left off.

## Command line

The same exe is also the command-line tool when given a command. From PowerShell, pipe the output (`| Out-Host`) so the shell waits for it and shows the output.

```powershell
gamepass-downloader login
gamepass-downloader queue --json
gamepass-downloader streaming <product or content id> <folder> --market US --parallel 8
gamepass-downloader adopt <content id> <folder>          # as administrator
gamepass-downloader verify "D:\Xbox\<game>\Content" --repair
gamepass-downloader --help
```

`verify` checks an installed game or a download against the package's hashes. With `--repair`, it fetches only the damaged pages again.

## Building

You need:

- Rust 1.99.0 with the MSVC toolchain.
- Visual Studio Build Tools (Desktop development with C++, including the Windows SDK).
- `protoc` and `cmake` on `PATH`.

```bat
build.cmd           :: target\release\gamepass-downloader.exe
build.cmd all       :: also target\release\xodus-cli.exe
build.cmd test      :: workspace tests
```

The exe links the C runtime statically and needs no other files beside it.

Releases are built by `.github/workflows/release.yml`. Set `version` in `crates/gamepass-gui/Cargo.toml`, then push a matching tag (`git tag -a v0.1.0 -m "gamepass-downloader 0.1.0"`, `git push origin v0.1.0`). The workflow builds on a Windows runner and checks the exe imports only Windows system DLLs. It then publishes the exe and its checksum as a GitHub release. A tag with a suffix, such as `v0.2.0-rc1`, becomes a pre-release.

`AGENTS.md` describes the design and the Xbox app internals the hand-off relies on. `HANDOFF.md` is the development log. `UPSTREAM.md` explains how to take in new upstream Xodus and Nexus releases.

## What it will not do

- Download anything your account isn't entitled to, skip the licence check, or work around device limits.
- Output content keys or package URLs for other tools. Upstream's `license`, `sp-license` and `download --dry-run` commands are removed.
- Launch games, or decrypt or place the game executables that the Xbox app protects.

## Credits and licence

This project is built on [Xodus](https://github.com/xodus-gaming/xodus) (GPLv3), which provides the sign-in, licensing and MSIXVC streaming code. It also applies the backend patch series from [Xodus Nexus](https://github.com/domsvoboda/xodus-nexus) (MIT). The git history keeps upstream's commits and one commit per Nexus patch.

gamepass-downloader is licensed under the [GNU General Public License v3.0](LICENSE).

This is an unofficial tool, not affiliated with or endorsed by Microsoft. Xbox, Game Pass and Windows are trademarks of Microsoft.
