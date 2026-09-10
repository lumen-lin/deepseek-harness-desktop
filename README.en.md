[![中文](https://img.shields.io/badge/中文-red.svg)](README.md)
[![English](https://img.shields.io/badge/English-blue.svg)](README.en.md)


# DeepSeek Harness Desktop

A lightweight Windows desktop shell built with [Tauri 2](https://tauri.app) that launches the [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) Web UI locally.

Tiny footprint, one-click launch. The shell works well for daily use and covers the core web features.

> ⚠️ **Unofficial project**: This is a personal, non-official desktop shell, not affiliated with DeepSeek. It is only a "launcher" — the actual dsh core is the official repository you clone locally.

<img width="2151" height="1339" alt="image" src="https://github.com/user-attachments/assets/d5ad9f97-c8bc-448d-a50c-74e29c1813bf" />


## What it is / what it is not

| | Description |
|----|----|
| ✅ What it is | A lightweight launcher: locate the `deepseek-harness` repository → start its `dsh web` server → load the official Web UI in a window |
| ❌ What it is not | Not a standalone AI chat app. It does not bundle the dsh core, nor does it include any models |

The shell itself is only ~5 MB (Tauri reuses the system WebView2 for rendering). **Runtime dependencies**:

- The `deepseek-harness` official repository
- Node.js 22.19+ (24+ recommended)
- WebView2 (built into Windows 10/11; older systems will be guided through the download during installation)
- (Optional, only needed for the update feature) git + pnpm

## Installation

Download `DeepSeek Harness_<version>_x64-setup.exe` from [Releases](../../releases) and double-click to install (per-user install, no admin rights needed).

Double-click the installer icon

<img width="256" height="256" alt="Official icon" src="https://github.com/user-attachments/assets/280bac0e-ded7-4ff5-b1a0-1284985ec31e" />


## Usage

1. Prepare the official repository:
   ```sh
   git clone https://github.com/deepseek-ai/deepseek-harness.git
   cd deepseek-harness
   pnpm install
   pnpm run build
   ```
2. Launch DeepSeek Harness. On first start, if the repository is not auto-detected, a dialog will ask you to **manually select the repository root 'deepseek-harness'** .

<img width="1246" height="694" alt="QQ_1787390031188" src="https://github.com/user-attachments/assets/556b83c5-65c4-4d88-8134-8d3a3f518879" />


3. Configure your model API key in the UI, pick a workspace, and start using it.
4. Choose your language

<img width="2152" height="1344" alt="image" src="https://github.com/user-attachments/assets/3f5e4bdc-679a-4e4a-ac41-45c8879241e1" />


Follows the official dark/light theme

<img width="2149" height="1338" alt="image" src="https://github.com/user-attachments/assets/209135ee-8fab-4c01-86f6-73a4362df999" />




Task confirmation prompt before exit

<img width="2144" height="1339" alt="image" src="https://github.com/user-attachments/assets/dae3c05a-8118-4540-9dfe-e8c14a029207" />





## Updating dsh
Click the "DeepSeek Harness" title-bar item to view the shell version, dsh version, and repository location.

<img width="2154" height="1340" alt="image" src="https://github.com/user-attachments/assets/23d003bb-58fa-49fa-93d7-34c2ebe5c779" />

About DeepSeek Harness

<img width="2144" height="1338" alt="image" src="https://github.com/user-attachments/assets/2b2e74fa-9d67-470a-bd5c-8d1b742a0fcc" />

Check & Update Harness
<img width="2149" height="1344" alt="image" src="https://github.com/user-attachments/assets/7d6338b9-d607-4ce4-bc05-7f11ebd5b9ad" />


<img width="2143" height="1340" alt="image" src="https://github.com/user-attachments/assets/ca306e35-cc8c-4dc5-83a7-6c0495991800" />



The shell has a built-in "Check & Update Harness" (Help menu) that updates the dsh core to the latest official version, in four steps:

1. Pull the latest code (`git pull`)
2. Install dependencies (`pnpm install`)
3. Clear stale build output (`pnpm run clean`)
4. Rebuild (`pnpm run build`)

The dropdown lets you choose a target version: it defaults to upstream `master` (a **fast-forward pull**, so commits you made locally are never overwritten), and you can also switch to a historical tag — including downgrading to an older stable release, which uses `git checkout -B dsh-selected <tag>`.



<img width="2151" height="1344" alt="Update log" src="https://github.com/user-attachments/assets/f333d133-b4cb-4ae1-82b5-1661aa214d4d" />

- **Success**: the shell window stays open — click "Restart Server" on the update page to launch the new version
- **Failure**: the old server is restored automatically, and you can keep using it (full output in logs). If the source had already moved to the new version but the build failed, the shell **rolls the source back and rebuilds**; versions that fail to build are blacklisted and flagged on the next check
- **Force Rebuild**: for when a build was interrupted and the artifacts no longer match the source — it re-installs dependencies and rebuilds without touching the source version

> Note: this updates the **dsh core**, not this shell. For shell updates, download the new installer from Releases.

## Building from source

```sh
cd src-tauri
cargo tauri build
```

Output goes to `target/release/bundle/nsis/`. Requires the Rust toolchain (edition 2024, rustc 1.85+).

> The icon `src-tauri/icons/icon.ico` is generated from `256x256.png` by `gen_installer_assets.py`
> (needs Python + Pillow). It is already committed, so day-to-day builds skip this script.

## FAQ

**"Repository not found" prompt?**
The shell searches upward from the exe location for the `deepseek-harness` directory; if not found, it asks you to pick it manually. You can also set the `DSH_REPO` environment variable to point directly at the repository path.

<img width="1246" height="694" alt="QQ_1787390031188" src="https://github.com/user-attachments/assets/ea68342b-c9b1-4ebd-9718-cfd8a193bb20" />


## Project structure

```
.
├── src-tauri/             # Rust shell (modular)
│   ├── src/
│   │   ├── main.rs        # Entry: window, repo location, event routing
│   │   ├── commands.rs    # Tauri commands (version/open dir/quit…; all check the call token)
│   │   ├── server.rs      # dsh web subprocess startup, readiness, port fallback
│   │   ├── shell.rs       # Local HTTP server for the shell page (same-site hosting + call token)
│   │   ├── nav.rs         # Navigation allow-list: main window may only stay on our own ports
│   │   ├── update.rs      # Auto-update flow (pull/switch + pnpm install/clean/build)
│   │   ├── theme.rs       # Theme following (settings.yaml watcher)
│   │   ├── locale.rs      # Language following (settings.yaml watcher)
│   │   ├── logging.rs     # Logging
│   │   ├── paths.rs       # App base dir / home / dsh config dir
│   │   └── repo.rs        # Repository locating & shell data dir
│   ├── tauri.conf.json    # Bundling config
│   └── capabilities/      # Permission config (ACL is only the first layer — see src/shell.rs)
├── windows/installer.nsi  # NSIS script (Tauri upstream template + small customizations)
└── ui/index.html          # Shell page (custom title bar + loading page + iframe, bilingual)
```

## Security boundaries (the short version)

The shell page is served from `127.0.0.1`, while Tauri's ACL can only authorize by
source host:port — it cannot tell the shell page apart from another same-origin page
served by any other local process. Two real boundaries make up for that:

1. **Call token** — the shell page URL carries a random `?k=`; every custom command verifies it,
   and the token is only handed out together with the shell page on its secret path;
2. **Navigation allow-list** (`nav.rs`) — the main window may only stay on ports started by this
   process; anything else is handed to the system browser.

Read the header comments in `src/shell.rs` and `src/nav.rs` before touching this area.
