# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Web client translations**: The web client is localized with i18next, with English and German translations in `crates/rustyfin-web/src/translations/` (one JSON file per language, English as the source). The display language follows the browser and can be changed in the setup wizard and in the user settings; picking it in the wizard switches the UI immediately. Previously the UI was German only.

### Changed
- **pnpm for the web client**: `crates/rustyfin-web` uses pnpm instead of npm. `package.json` pins the version in `packageManager` (Corepack and `pnpm/action-setup` read it), and `pnpm-lock.yaml` replaces `package-lock.json` with the same dependency versions. The Dockerfile, the release workflow, `build.sh`, `start.sh` and the README use pnpm.
- **Access tokens survive restarts**: Access tokens are now opaque values stored server-side, one per device, instead of self-contained JWTs signed with a per-start secret. A restart no longer logs anyone out, and the server id is stable across restarts (persisted to `<data>/server_id`). `RUSTYFIN_SECRET` is now ignored (with a one-time startup notice). **Upgrade note:** tokens issued by an older build stop working once, at the upgrade — every client signs in again after updating.
- **Stricter login rate limit**: The login, forgot-password and reset endpoints now allow 10 requests per 60 seconds per client IP by default (was 500/60 s), tunable with `RUSTYFIN_RATE_LIMIT` and `RUSTYFIN_RATE_WINDOW` (`0` disables). `X-Forwarded-For` is trusted only from a proxy listed in the new `RUSTYFIN_KNOWN_PROXIES` (comma-separated IPs or CIDRs; loopback only when listed); otherwise the connecting peer address is used, so a spoofed header can no longer claim the loopback exemption or rotate past the limit.

### Fixed
- **Startup wizard metadata settings**: The wizard now saves the chosen metadata language and country. It sent them as `preferred_metadata_language` and `metadata_country_code`, which `POST /Startup/Configuration` ignores because it expects PascalCase fields.
- **Setup wizard owner account**: The server no longer creates an `admin`/`admin` account at start. The setup wizard creates the first administrator, the server owner, with the typed name and password. Before, the wizard only changed the default admin's password, ignored the typed name and hid sign-in failures. The `/Startup/*` endpoints also stayed anonymous, so anyone could reset the administrator's password. Now:
  - `POST /Startup/User` works only while no owner exists, and the `/Startup/*` endpoints need an administrator afterwards.
  - `StartupWizardCompleted` means that an owner exists.
  - The owner cannot be deleted or demoted.
  - On upgrade, the earliest administrator of a completed setup becomes the owner.
  - A leftover `admin` account whose password is still `admin` is disabled once the owner is created under another name.
  - The wizard shows every failed step and resumes there on retry.
  - `start.sh smoke` creates a throwaway owner in a temporary data directory.
- **Library folders**: Creating a library now checks its folders. Before, `POST /Library/VirtualFolders` stored any path, so the setup wizard and the library dialog accepted a folder that does not exist, and the library's scan found nothing. Now each path must be a full path to a folder that exists on the server and that the server can list. Otherwise the server stores nothing and answers 400 with a message that names each bad path and the reason, for example `The folder "/media/films" was not found on the server.` In Docker, the paths are the ones inside the container.
- **LAN admin takeover via the reset PIN (R1-01)**: The forgot-password flow now follows Jellyfin's. A reset PIN is issued only to a request from the local network and only for an existing user, written to a file in the data directory (mode 0600), and never returned or logged; the response is uniform and does not reveal whether a user exists. The PIN is random, stored only as a hash, valid for 30 minutes, single-use, and voided after five wrong attempts. The old biased PIN generator was removed (M-17).
- **Access tokens can be revoked (R1-08, R1-09)**: Logging out ends that device's token, changing a password ends the user's other tokens, and disabling or deleting a user ends all of theirs; a disabled user's existing tokens are rejected. Restarts keep valid tokens valid.
- **`admin`/`admin` after an upgrade**: At startup the server logs a warning naming any enabled administrator whose password is still `admin`.
- **Password hashing no longer blocks the server (M-15)**: argon2 hashing and verification run on a blocking thread pool, so a burst of logins no longer stalls unrelated requests.
- **Login response reflects the current login (M-16)**: `AuthenticateByName` now returns the user with the `LastLoginDate` set by that login, not the previous one.
- **Proxy address handling**: `X-Forwarded-For` is read across all header lines, right to left, with `ip:port` and `[v6]:port` entries; loopback is trusted as a proxy only when listed in `RUSTYFIN_KNOWN_PROXIES`, and a malformed entry no longer resolves to a client-chosen address. The reset-PIN LAN check uses the same client address, IPv4-mapped IPv6 peers count as their IPv4 address, and IPv6 clients share one login budget per /64. Before, a client behind a same-host proxy could claim the loopback exemption.

## [0.14.0] - 2026-11-03

### Fixed
- **Clippy `collapsible_if` warnings**: Collapsed nested `if let` statements in `scanner.rs` and `items.rs` to comply with strict Clippy rules (`-D warnings`). Ensures CI passes with `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

## [0.13.0] - 2026-11-03

### Changed
- **Additional performance optimizations**: Further improvements to reduce latency, eliminate redundant queries, and optimize hot paths.
  - **TV scanner version detection**: Pre-load all episodes into a HashMap for O(1) lookups, eliminating N+1 database queries during TV library scans (matching the movie scanner optimization).
  - **Episode enrichment**: Eliminated duplicate Season database query by loading Season once and reusing it for both series info and parent image tags (3→2 queries per episode).
  - **Filter query cache**: Replaced `std::sync::Mutex` with `parking_lot::Mutex` to avoid blocking Tokio worker threads under high concurrency.
  - **Blurhash HTTP client reuse**: `compute_blurhash` now accepts an optional shared HTTP client, eliminating per-request client creation overhead for remote images.
  - **`normalize_name` optimization**: Reduced string allocations from ~60 per call to ~2 by using a single buffer and processing tags in one word-based pass instead of 48x `.replace()` calls.
  - **L1 cache TTL**: Increased item details cache TTL from 30 seconds to 5 minutes, improving cache hit rate for frequently accessed items.
  - **Transcode manager**: Replaced `RwLock<HashMap>` with `DashMap` for lock-striping, reducing contention under concurrent transcoding workloads.

## [0.12.0] - 2026-11-03

### Changed
- **Performance optimizations**: Comprehensive improvements across all layers to reduce latency and eliminate bottlenecks.
  - **Scanner version detection**: Pre-load all existing movies into a HashMap for O(1) lookups, eliminating N+1 database queries during flat library scans.
  - **Image proxy**: Reuse shared HTTP client for remote image downloads instead of creating a new client per request.
  - **Item enrichment**: Parallelize 6 independent database queries (images, chapters, trickplay, user data, people, studios) using `tokio::join!` instead of running them sequentially.
  - **Rate limiter**: Replaced `Mutex<HashMap>` with `DashMap` for lock-striping, reducing contention under high concurrency.
  - **Batch inserts**: Disable filter triggers during batch media item inserts to avoid write amplification from per-row COUNT(*) queries.
  - **Metadata refresh**: Increased concurrent API task semaphore from 5 to 10, better utilizing TMDb's 20-40 req/s rate limit.
  - **Blurhash pre-computation**: Added `blurhash` column to `item_images` table; blurhash is now computed during metadata refresh and stored in the database, avoiding on-demand computation on every item detail request.
  - **Transcode cache cleanup**: Added a periodic background task (every 15 minutes) that automatically removes stale transcode sessions (older than 1 hour) and cleans up orphaned cache directories.
  - **Search performance**: Replaced slow LIKE substring queries with FTS5 full-text search across name, overview, and genres. LIKE fallback now uses prefix matching (`query%`) so B-tree indexes can be used when FTS5 is unavailable. Added `idx_media_items_name` and `idx_media_items_type_name` indexes.

## [0.8.0] - 2026-11-03

### Changed
- **Database migrations**: Replaced manual migration execution with SQLx's built-in `Migrator` for cleaner, tracked database migrations.
  - SQLx now tracks applied migrations in the `sqlx_migrations` table, ensuring each migration runs exactly once.
  - Renamed migration files to have unique sequential numbers (001–027), fixing the duplicate `002_` prefix.
  - Reduced `lib.rs` from 245 to 115 lines by replacing 26 manual `include_str!()` calls with a single `MIGRATOR.run()` call.

## [0.7.0] - 2026-10-29

### Added
- **Comprehensive test suite**: Added 200+ new tests across the workspace, bringing total test count to 318.
  - **API integration tests** (57 tests): Auth, library, search, system, DLNA, items, sessions, playback, users, Live TV, and more.
  - **Unit tests for route helpers** (91 tests): DLNA XML escaping, UPnP classes, device detection, auth header parsing, library views, range parsing, MIME type detection.
  - **Model tests** (88 tests): MediaType, LibraryContentType, ImageType, SegmentType, ProviderIds, and more.
  - **Core library tests** (19 tests): Subtitle SRT-to-VTT conversion and metadata filename cleaning.
  - **Auth tests** (16 tests): Password hashing, JWT token creation/validation, refresh tokens, PIN generation.
- **Test coverage improved**: Overall line coverage increased from 14.88% to 24.14%.

### Changed
- **`libraries_to_views` visibility**: Changed to `pub(crate)` in `library.rs` to allow unit testing.

## [0.6.0] - 2026-10-29

### Added
- **Cross-platform binary releases**: GitHub Actions workflow builds and packages binaries for 5 platforms on every push to `master` and on published releases.

### Changed
- **TLS implementation**: Switched `reqwest` from `native-tls` (OpenSSL) to `rustls-tls` for reliable cross-compilation without system OpenSSL dependencies.
- **Disk usage API**: Made `disk_usage()` cross-platform compatible with `#[cfg(target_family = "unix")]` guard and non-Unix fallback.
- **Windows cross-compilation**: Added `cargo-xwin` support for building Windows binaries from Linux CI runners.
- **macOS builds**: Native builds on macOS GitHub runners (x86_64 and aarch64), as `cross` cannot cross-compile to macOS.

### Fixed
- **aarch64-unknown-linux-gnu build**: Resolved OpenSSL version mismatch by using `rustls` instead of `native-tls`.
- **x86_64-pc-windows-msvc build**: Configured `cargo-xwin` with required LLVM/clang dependencies for Windows cross-compilation.
- **macOS aarch64 build**: Moved from `cross` fallback to native macOS runner builds.

## [0.5.0] - 2026-10-29

### Added
- **Cross-platform release builds**: GitHub Actions workflow (`release.yml`) uses `cross` to build binaries for 5 platforms (Linux x86_64/aarch64, Windows x86_64, macOS x86_64/aarch64)
- **Release artifacts**: Each binary is packaged with the embedded web client for standalone distribution

### Changed
- **Code formatting**: Applied `cargo fmt` to fix line-length violations across API and DB crates

### Other
- **Build script** (`build.sh`): Automated build pipeline with Quality Gates (fmt, clippy, test), WebUI build, and release build
- **systemd service** (`rustyfin.service`): Production-ready service file with hardening (ProtectHome, ProtectSystem, NoNewPrivileges)
- **One-command install**: `sudo ./build.sh --install` installs binary, creates system user, and sets up systemd service

## [0.4.0] - 2026-10-28

### Added
- **Extended metadata from TMDb**: Movie and TV details now include status, original language, tagline, production countries, budget, revenue, homepage, vote count, and popularity
- **Studios from TMDb**: Studios are now fetched and stored in `studios`/`item_studios` tables
- **People for all media types**: Cast and crew data is now loaded for movies, episodes, and series (not just audio)
- **People batch query**: Batch enrichment now loads all people in a single query instead of N+1
- **Service Worker**: Offline caching for static web assets (JS, CSS, images)
- **Infinite Scroll**: Automatic loading when scrolling near the bottom, replacing "Load More" buttons
- **Virtual Scrolling**: Only visible cards are rendered in the library view (optimized for 1500+ items)
- **Vite build system**: Hot-reload development server (`npm run dev`) and optimized production builds

### Changed
- **WebUI performance optimizations**:
  - HLS.js loaded locally instead of from CDN (~200-500ms faster startup)
  - Image requests optimized: full-resolution images loaded on hover only
  - DOM rendering uses `DocumentFragment` instead of `innerHTML`
  - Template caching for item cards (~30% faster rendering)
  - Search input debounced (300ms delay)
  - CSS `backdrop-filter` only on desktop (mobile performance)
  - Hardware-accelerated CSS animations with `will-change`
- **Cache version automation**: MD5 hash of all assets is automatically computed on build and injected into Service Worker and HTML
- **Migration 025**: New DB columns for extended metadata (status, budget, revenue, homepage, vote_count, popularity)

### Fixed
- **Movie detail page**: Missing backdrop image, cast, and studio data
- **Similar recommendations**: Inconsistent card sizes (mixed movies and episodes in same row)
- **Test compilation**: `hash_combine` and `filter_cache_key` now accessible from test module
- **API tests**: Missing `http_client` field in test `AppState`
- **Migration 025**: Now registered in `lib.rs` so test databases include new columns

### Removed
- **Manual build script**: `build.js` replaced by Vite configuration (`vite.config.mjs`)

### Other
- **Code quality**: All `cargo fmt`, `cargo clippy`, and `cargo test` gates pass with zero warnings
- **Clippy allowances**: Added `#[allow(dead_code)]` for TMDb deserialization structs and `#[allow(clippy::too_many_arguments)]` for SQLx query functions
