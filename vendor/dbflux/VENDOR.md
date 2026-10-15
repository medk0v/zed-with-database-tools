# DBFlux database libraries

Source: https://github.com/0xErwin1/dbflux
Revision: b8427ac8818673a46369b7dea090438223c7b125
License: MIT OR Apache-2.0 (license files included).

This is the dependency closure needed by the PostgreSQL, MySQL and SQLite drivers, including their SSH support. No DBFlux application UI or GPUI fork is included.

Integration adjustments:
- Workspace members are limited to the included crates. Upstream development dependencies are omitted because the shared upstream test helpers require every database driver and live-container test infrastructure. The Zed adapter has independent local integration tests.
- rusqlite is pinned to 0.32.1 to share Zed's libsqlite3-sys 0.30.1 native SQLite library. The SQLite cancellation guard calls its infallible progress_handler API without the Result handling introduced in newer rusqlite.
- dbflux_core uses Zed's tree-sitter Git revision so the native library and language types are shared.
- SSH host verification is changed from DBFlux's automatic TOFU/private-file migration to strict, read-only OpenSSH known_hosts checking. Unknown and changed keys are refused. This removes all DBFlux storage access from the embedded SSH path; storage/approval crates are omitted. The driver connection APIs receive secrets explicitly from Zed's credential flow.
- Zed's Cargo configuration prefers dynamic libssh2 via pkg-config. On macOS this keeps libssh2's OpenSSL calls isolated from WebRTC's static BoringSSL, whose incompatible symbols otherwise break SSH key exchange. The macOS packaging script requires the dynamic library, verifies the executable imports it, and bundles its native dependency closure with app-relative paths. SSH authentication and host verification remain in the existing DBFlux integration.
- PostgreSQL direct/tunnel keyword connections explicitly set protocol sslmode=require for require/verify-ca/verify-full, preventing the underlying client default (Prefer) from accepting a server TLS refusal. Unknown modes are refused by the adapter before connecting.
- The SSH include glob test uses distinct basenames so it also runs on case-insensitive macOS volumes, while still checking case-sensitive extension matching.
- The Connection trait and PostgreSQL/MySQL/SQLite drivers expose execute_atomic_row_changes for generated grid edits. Each batch uses a native transaction, requires exactly one affected row per statement, rolls back on conflict/error/cancellation, and refuses to commit an existing user transaction. MySQL grid editing requires InnoDB. The Zed adapter supplies identifier-safe SQL, lossless value literals, primary-key validation and original-value predicates.
