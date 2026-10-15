> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed with Database Tools

Zed with built-in database tools powered by DBFlux and inspired by DataGrip.

This fork of Zed **v1.23.3** brings database connections, a schema browser, editable data grids, and SQL consoles into the editor. The database tools are integrated into the application rather than distributed as a separate extension. The application name remains **Zed**.

## What's implemented

### Database connections

- PostgreSQL, MySQL, and SQLite support through vendored DBFlux drivers.
- Saved connection profiles with global or project-specific visibility.
- Connection colors shared by the sidebar, data tabs, and SQL consoles.
- Read/write and read-only access modes.
- TLS options and SSH tunnels for PostgreSQL and MySQL, including key, agent, and password authentication.
- Database passwords and SSH secrets stored through the system credential store; on macOS, this uses Keychain. Connection metadata is stored separately without passwords.
- Strict SSH host-key checking against existing OpenSSH `known_hosts` files. Unknown or changed host keys are refused.

### Schema browser and column editing

- Expand tables using their **arrow buttons**. Clicking a table name opens its data without toggling the tree.
- Browse **columns**, **keys**, **foreign keys**, **indexes**, and **checks**, with item counts.
- Expand individual columns with their arrows to inspect type, nullability, default value, and primary-key status.
- Open a column editor from its name or pencil button. Rename columns and change PostgreSQL/MySQL column types through **Preview SQL → Apply**.
- Rename SQLite columns; changing their types is not supported by this editor because it requires rebuilding the table.
- Drag columns within the schema tree or drag grid headers to change their display order. The order is synchronized across the tree and open data tabs for the same connection and table during the app session. This does not change the database's physical column order.
- Refresh open table metadata after schema changes. Pending row edits are preserved but cannot be applied against an outdated schema; revert them to reload.

### Table browsing and editing

- Paginated browsing with row counts and selectable page sizes from 100 to 10,000 rows; the default is 500.
- Server-side `WHERE` filtering, plus sorting and column filters for the fetched page.
- Resizable columns, row selection, cell copying, and context menus.
- Inline cell editing, row insertion/deletion, and bulk assignment of values, `NULL`, or `DEFAULT`.
- Staged changes with **Apply**, **Revert**, and **⌘S**. Each saved batch runs in one transaction, checks original row values, and rolls back on conflicts or errors.
- Background data loading and query cancellation.
- Table/view context menus for opening data, preparing a `SELECT`, copying names, refreshing, and confirmed deletion.

Grid editing requires a usable primary key and supported value types. Views and unsupported tables remain browsable. MySQL grid writes require InnoDB.

### SQL consoles

- A console associated with each connection, with SQL syntax highlighting and completion for keywords and known table/view names.
- Run the selected statement, or the console text if nothing is selected, using **Run** or **⌘Enter**.
- View query results and affected-row counts; cancel an active query with **Cancel** or **⌘.**.
- Result sets are limited to 500 fetched rows. Arbitrary query results are read-only grids; open a table to edit its rows.

Connections default to **read/write**. Grid edits are staged until Apply, while SQL writes execute immediately when Run is pressed. The result-row limit does not limit rows affected by a write. Choose **Access: read-only** to block writes. Table/view deletion and applied schema changes are separate from staged row edits and cannot be undone with the grid's Revert button.

### Changes to the editor

- Removed AI assistants, AI edit predictions, account/login flows, and collaboration controls from startup and visible UI, including their settings and key bindings.
- Retained ordinary editor and language-server completion.
- Disabled usage telemetry, diagnostic uploads, and official Zed automatic updates.
- Restored the native title bar and macOS window controls.
- Kept this fork's configuration separate from official Zed. On macOS it uses `~/.config/zed db/` and bundle ID `local.zed-db.ZedDB`.

Some upstream AI, account, and collaboration infrastructure remains in the source tree and transitive dependencies.

## Build a release on macOS

Prerequisites: Xcode development tools, a working Metal toolchain, CMake, Rust **1.99.0**, and Homebrew packages `pkg-config`, `libssh2`, and `openssl@3`. The first build needs network access for dependencies.

From the repository root:

```sh
brew install pkg-config libssh2 openssl@3
./script/bundle-zed-db-mac --release
open "outputs/Zed.app"
```

The script builds and packages an optimized, ad-hoc signed application at `outputs/Zed.app`. Quit any older running copy before opening it. To install it, move the generated `Zed.app` into `/Applications`. The script itself does not install, publish, or notarize the application.

Additional options:

- `--release --no-build`: package existing release binaries from `target/release`.
- `--output "/absolute/path/Zed.app"`: choose a different bundle destination.
- Omit `--release` for a development build.
- Set `ZED_DB_RUST_TOOLCHAIN` to override the Rust toolchain.

The bundle includes its native SSH dependencies, so the packaged application does not require Homebrew. Dynamic libssh2 keeps its OpenSSL symbols separate from WebRTC's BoringSSL; do not set `LIBSSH2_STATIC` or `LIBSSH2_NO_PKG_CONFIG` when building. See [FORK.md](FORK.md) for additional build and usage details.

This fork's packaging workflow has been validated on macOS. Upstream development instructions are available for [macOS](docs/src/development/macos.md), [Linux](docs/src/development/linux.md), and [Windows](docs/src/development/windows.md); they are not verification of this fork on every platform.

## Getting started

1. Open **Databases** and click **+**, or run **database: new connection** from the command palette.
2. Choose PostgreSQL, MySQL, or SQLite and enter the connection details. SQLite requires an absolute path to an existing database file.
3. Choose the access mode, optional SSH/TLS settings, color, and project visibility, then select **Save and Connect**.
4. Use the arrow beside a table to inspect its structure, click its name to browse data, or use the connection's terminal button to open a SQL console.

## Current scope

This is an early database client inspired by DataGrip's workflow. It does not provide a full schema designer, ER diagrams, a migration workflow, database-aware refactoring, or DataGrip-level contextual SQL completion.

Local SQLite checks cover table editing, conflict handling, column reordering, and column renaming. The project includes backend and GPUI regression tests. Live PostgreSQL/MySQL/SSH behavior has not been verified against external servers in this implementation, and the final UI check of automatic schema refresh across multiple open tabs remains incomplete.

## Credits and licensing

The editor is based on [Zed](https://github.com/zed-industries/zed). Database drivers and SSH support are adapted from [DBFlux](https://github.com/0xErwin1/dbflux); see [vendor/dbflux/VENDOR.md](vendor/dbflux/VENDOR.md) for the pinned revision, local adaptations, and included licenses. DataGrip is the workflow reference for the database UI.

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).
