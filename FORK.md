# Zed

This is a local fork of Zed **v1.23.3** with a native database panel, table editor, and SQL console for PostgreSQL, MySQL, and SQLite. It is an early database client with a smaller feature set than DataGrip.

**Connections default to read/write. Table changes are staged until Apply or ⌘S; SQL writes run immediately when you press Run or ⌘Enter.** Choose **Access: read-only** in a connection's settings to block writes. Grid editing requires a primary key and values that can be represented without loss; views and unsupported tables remain available for browsing. MySQL grid writes require an InnoDB table.

## Build and package on macOS

Install the Xcode development tools, a working Metal toolchain, CMake, Rust 1.99.0, and the native SSH libraries (`brew install pkg-config libssh2 openssl@3`). The first build needs network access to fetch dependencies. From this repository (`/Users/medk0v/Developer/other/zed` in this checkout):

```sh
CARGO_BUILD_JOBS=6 cargo +1.99.0 build -p zed -p cli --features util/debug-embed
./script/bundle-zed-db-mac --no-build
open "outputs/Zed.app"
```

Alternatively, `./script/bundle-zed-db-mac` builds and packages in one step. Use `--release` for an optimized build, or `--output "/absolute/path/Zed.app"` to choose the destination. Set `ZED_DB_RUST_TOOLCHAIN` to override the script's toolchain. The script creates an ad-hoc signed app with the upstream black stable Zed icon; it does not install, publish, or notarize it. Quit an older running copy before opening the rebuilt app by its explicit path.

SSH uses dynamic libssh2 so its OpenSSL calls remain isolated from WebRTC's statically linked BoringSSL. Mixing those libraries inside the executable caused SSH key exchange to fail. Repository Cargo configuration selects dynamic libssh2 through pkg-config for ordinary `cargo build` commands as well. The macOS packaging script refuses an executable that fell back to static libssh2, copies the native libraries and their dependencies into the app, and rewrites their references before signing; the packaged app does not need Homebrew. Do not set `LIBSSH2_STATIC` or `LIBSSH2_NO_PKG_CONFIG`. If rebuilding reuses an old static native build, run `cargo clean -p libssh2-sys` and rebuild.

The app is displayed as **Zed**. Its stable bundle ID remains `local.zed-db.ZedDB`, and its default macOS configuration directory remains `~/.config/zed db/` (including the space), preserving this fork's saved settings and connections separately from official Zed. Existing official Zed settings are not imported automatically. The packaging script refuses to replace an app with an unrelated bundle ID.

## Connect

1. Open the command palette and run **database: new connection**, or use **+** in the **Databases** panel. **database: toggle focus** focuses the panel.
2. Choose PostgreSQL, MySQL, or SQLite. For SQLite, enter an absolute path to an existing database file. For PostgreSQL or MySQL, enter the database host, port, database, user, password, and optional TLS mode. Choose the connection's access mode.
3. For an SSH tunnel, enable SSH and enter the SSH host, port, and user. Choose a private key, leave the key field empty to use an SSH agent, or choose password authentication. Supply the key passphrase or SSH password when required. The database host is the host reachable from the SSH server.
4. Select **Save and Connect**. Click a table or view to browse it, or use the connection's terminal button to open a SQL console.

Connection metadata is saved in `~/.config/zed db/database-connections.json`. Database passwords and SSH secrets use macOS Keychain, not that JSON file. A blank password field preserves the previously saved password.

Right-click a connection to **Connect** or **Disconnect**, **Edit connection…**, open a **New SQL console**, **Refresh tables**, set its **Color** or **Visible in** scope, or **Remove connection from list…**. Removing a connection removes its saved profile; it does not run `DROP DATABASE` or delete database data.

Choose red, orange, yellow, green, blue, purple, or no color. The color identifies the connection in the panel and its table/console tabs; changing it updates already-open tabs.

**Visible in → All projects** makes a connection global. **This project** matches the exact set of normalized local project-root paths, including every root of a multi-folder project. Root order and redundant path components do not change the match; adding or removing a root does. New connections default to the current local project, while older profiles without a scope remain global. Remote projects and windows with no local folder offer only **All projects**.

SSH host keys must already be verified in `~/.ssh/known_hosts` or `/etc/ssh/ssh_known_hosts`. Unknown or changed host keys are refused. Verify the server fingerprint through your usual SSH workflow before connecting; the database UI does not approve host keys automatically. SQLite opens local files; SSH tunnels apply to PostgreSQL and MySQL.

## Browse and edit tables

The grid supports column resizing, sorting, filtering, selection, and copying. Tables open with 500 rows per page. The footer lets you choose 100, 500, 1,000, 2,000, 5,000, or 10,000 rows for the current table tab; changing the size returns to the first page. It also shows the displayed row range, total matching rows, and current/total pages. Use **First**, **Previous**, **Next**, and **Last** to navigate. Use a SQL expression in **WHERE**, then **Filter**, to filter on the server; the total and page count follow that filter. Counts and rows load in the background and can be cancelled. Refreshing after rows are removed moves an out-of-range page back to the last available page. Column-header filters and sorting affect only the current fetched page.

Right-click a table or view in the Databases tree to **Open data**, open a prepared **SELECT in SQL console**, **Copy table name**, or **Refresh tables**. **Delete table…** and **Delete view…** require confirmation and are disabled for read-only connections. These commands immediately run `DROP TABLE` or `DROP VIEW`, without `CASCADE`; they are separate from staged row deletion and cannot be undone with **Revert**.

- Double-click a cell and edit its value. **Enter** or leaving the cell stages the edit; **Esc** cancels it. An unchanged NULL or DEFAULT cell keeps its original meaning. To replace one with an empty string, use the bulk **Value** mode with an empty value field.
- Use **⌘-click** to toggle rows and **Shift-click** to select a range. **Select page** selects the currently visible rows of the loaded page, not all matching rows in the database.
- Right-click a cell or row number for **Copy cell**, **Add row**, **Delete selected**, **Set column to NULL in selected rows**, **Apply**, **Revert**, and **Refresh**. Right-clicking within the selection keeps all selected rows; right-clicking another row selects that row. Add, delete, and NULL actions stage changes until Apply. Cell-specific commands require a cell click, and NULL is available only for nullable columns.
- To change several rows together, select them, choose a column beside **Set**, choose **Value**, **NULL**, or **DEFAULT**, and click **Set selected**. Value mode uses the entered text; NULL and DEFAULT have SQL semantics.
- **Add row** stages a new row whose omitted values use database defaults, clears the local column filters, and scrolls to the new row. The server's WHERE expression stays unchanged. Fill any required columns. **Delete selected** stages deletion of the selected rows.
- **Apply** or **⌘S** includes the active cell's current edit and saves all staged inserts, updates, and deletions in one transaction. The original row values are checked to detect concurrent changes. If a row changed or any write fails, the whole batch rolls back and the pending changes remain for review.
- **Revert** discards staged changes. Apply or revert before refreshing, changing the server filter, or moving to another page. **Cancel** requests cancellation of an active load or save.

Tables without a usable primary key, views, and tables containing unsupported values cannot be edited in the grid. The grid displays the reason. Binary values use hexadecimal text; database constraints still apply to values, NULLs, and defaults. A successful write may move a row out of the active filter or page when the table reloads.

## Run SQL

Run one selected SQL statement, or the console text when nothing is selected, with **⌘Enter** or **Run**. **Cancel** or **⌘.** requests cancellation. Result sets contain at most 500 fetched rows. Console completion offers SQL keywords and known table/view names; normal editor and language-server completion remains available for source files.

An arbitrary `SELECT` result is a result-only grid. Open the table itself from the Databases panel to edit rows. SQL `INSERT`, `UPDATE`, and `DELETE` execute immediately on a read/write connection and report affected rows; they do not enter the table editor's pending changes, and **Revert** cannot undo them. The result row limit does not limit how many rows a SQL write affects.

## Scope and validation

AI assistants, AI edit predictions, collaboration controls, and Zed account/login flows are removed from the app's startup and visible UI. Normal completions are retained. Official Zed automatic updates are disabled for this fork. Usage telemetry and diagnostic uploads are permanently disabled; settings cannot enable them, and their switches are removed.

The native title bar and macOS window controls are restored.

There is no schema designer, ER diagram, migration workflow, database-aware inspection/refactoring, or DataGrip-level contextual SQL completion. This fork still contains upstream AI, account, and collaboration source infrastructure and some transitive dependencies. Database drivers and SSH support are vendored from DBFlux; see [vendor/dbflux/VENDOR.md](vendor/dbflux/VENDOR.md) for provenance and local adaptations.

The previous macOS build was packaged, signature-checked, and launched with an isolated local SQLite profile. In the running app, inline edits, single-click Apply, ⌘S, WHERE filtering, bulk updates/deletions, new rows with defaults, Revert, unchanged NULL values, and SQL INSERT/UPDATE/DELETE were verified against the database file. An external concurrent change produced a visible conflict error, preserved pending edits, and left the server's values intact.

The new build was visually checked with a separate local SQLite profile: the title bar no longer overlaps the project name, connection colors persist, project A-only connections are absent from project B, and global connections remain visible in both. The grid context menu stages row deletion and Revert clears it without writing. The table deletion dialog names the table and connection; cancelling it leaves the table intact. The final bundle was rebuilt and signature-checked after correcting the tab renderer to preserve connection colors.

All 29 adapter tests pass, including atomic rollback, typed values, filtering, cancellation, profile persistence and scope, and table/view deletion. The staging integration test and four focused GPUI tests pass; the latter cover editing/focus, selection through sorting/filtering, revealing a new row after asynchronous mapping, and right-click menus preserving selection and underlying row identities while ordinary CSV previews retain right-click copying. Existing tabular-preview regressions also passed. The earlier SSH and telemetry suites remain part of the prior baseline. Live PostgreSQL, MySQL, and SSH connections have not been exercised against external servers.
