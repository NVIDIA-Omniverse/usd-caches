2.4.0-beta.5
------

### Updated
- OMPE-115306: Cleanup licensing for public release

2.4.0-beta.4
------

### Updated
- OMPE-112764: Update dependencies for Hub 2.4.0
- OMPE-114027: Cleanup Hub for public release
- OMPE-114369: Cleanup Hub protos and move to v1beta

2.4.0-beta.3
------

### Updated
- OMPE-108255: Sync docs from published into repo

### Fixed
- OMPE-97562: Improve uninstall.bat diagnostic output
- OMPE-108641: Caching files with same key but different extension should work

2.4.0-beta.2
------

### Updated
- OMPE-108544: Remove Cache Cleaner

2.4.0-beta.1
------

### Added
- OMPE-108252: Update Hub CI to automatically register NGC container in NSpect

2.3.0
------

### Updated
- Updated version number for release

2.3.0-beta.5
------

### Fixed
- OMPE-106530: Upgrade container from 4.0.8 to latest (4.1.1)

2.3.0-beta.4
------

### Fixed
- OMPE-106031: CVE-2026-42211: Update react-router to 7.18.2

2.3.0-beta.3
------

### Fixed
- OMPE-104568: Update vulnerable tokio-rs/bytes 1.7.0 (CVE-2026-25541)

2.3.0-beta.2
------

### Added
- OMPE-103428: Add support for Protobuf option into hub tests

2.3.0-beta.1
------

### Added
- OMPE-101636: Add protobuf support to Hub client

2.2.0
------

### Updated
- Updated version number for release

2.2.0-beta.3
------

### Fixed
- OMPE-97555: Hub CLI --show-config reports transport.tcp.ext_port: 0 while the running server uses Protobuf port 14093

2.2.0-beta.2
------

### Fixed
- OMPE-70364: Cache size and file count not updated in UI after clearing cache

2.2.0-beta.1
------

### Added
- OMPE-93148: Implement protobuf protocol over TCP

2.1.0-beta.2
------

### Updated
- OMPE-71609: Increase max_frame_length in Hub messagepack

2.1.0-beta.1
------

### Updated
- OMPE-86155: Integrate Chris Ruch's gRPC MR

### Fixed
- OMPE-86156: Allow debug logging in release builds

2.0.0
------

### Updated
- Updated version number for release
- Updated NGC container name from 'omni_hub' to 'hub_workstation_cache'

2.0.0-beta.5
------

### Fixed
- OMPE-83467: Update OSS dependencies for compliance

2.0.0-beta.4
------

### Fixed
- OMPE-82845: Use natively built binaries for docker deployment

2.0.0-beta.3
------

### Fixed
- OMPE-70364: Cache size and file count not updated in UI after clearing cache

### Added
- OMPE-82455: Publish Hub docker images to NGC

2.0.0-beta.2
------

### Added
- OMPE-78035: Hub needs a new Docker build target

2.0.0-beta.1
------

### Added
- OMPE-78845: Hub ARM build should be published to NGC

1.1.0
------

### Updated
- Updated version number for release

1.1.0-beta.5
------

### Fixed
- OMPE-69759: Ensure that cache-cleaner terminates when hub exits

1.1.0-beta.4
------

### Fixed
- OMPE-69585: Update hub packaging process to include cache-cleaner

1.1.0-beta.3
------

### Added
- OMPE-68921: Hub should start Cache Cleaner as a child process

1.1.0-beta.2
------

### Added
- OMPE-67882: Build web interface for Cache Cleaner

1.1.0-beta.1
------

### Added
- OMPE-66265: Add the Cache Cleaner package to Hub release package

1.0.2
------

### Updated
- Updated version number for release

1.0.2-beta.3
------

### Fixed
- OMPE-55891: Not able to uninstall Hub

1.0.2-beta.2
------

### Added
- OMPE-52740: Hub install scripts should support non-default locations

1.0.2-beta.1
------

### Fixed
- OMPE-41734: Fix default library root location for Hub on Windows
- OMPE-38510: hub.exe is causing workflow problems

### Added
- OMPE-38530: Promote Hub artifacts from dev to staging on ngc in pipeline

1.0.1
------

### Fixed
- Gitlab-CI should now use the correct version string for nSpect

1.0.1-beta.1
------

### Added
- OMPE-37798: Hub needs an uninstall script

1.0.0
------

### Added
- OMPE-24857: Create scripts for manual installation

1.0.0-beta.2
------

### Fixed
- Fixed licensing for zstr-rs and added a license file for the zstr library.

1.0.0-beta.1
------

### Added
- OMPE-24854: Update Hub CI flow to upload builds to NGC as well as packman

2024.1.0-beta.6
------

### Added
- OMPE-13923: Added visibility for showing cache size and total files in cache

2024.1.0-beta.5
------
### Fixed
- HUB-718: Hub stdout no longer included in the process that launched it.

### Added
- HUB-912: Added REST api to reset hub settings, `/api/1.0/service/reset`. Fixed reloading of tracing/logging when hub service was reset.
- OMPE-15441: Expose Hub HTTP port in API.
- OMPE-13923: Re-add the web page settings back to Hub.

2024.1.0-beta.4
------

### Fixed
- OMPE-2707: Do not hang on Hub client startup due to unhandled error(s)
- HUB-850: Hub client hangs if Hub service directory is not writeable

2024.1.0-beta.3
------

### Fixed
- OM-122769: Revert windows-rs to 0.52 due to Client Library linking issue on Windows

2024.1.0-beta.2
------

### Fixed
- OM-122769: Security updates

2024.1.0-beta.1
------

### Fixed
- HUB-920: Fixes for client and service restarts and crashes, leaving cache items in abandoned, unrecoverable state
- HUB-935: Ensure temporary hub config is written to same drive or mount point as final config path
- OM-119334: Hub unexpectled exits when background LRU thread cannot find cache entry

0.35.0-beta.0
------

### Fixed
- HUB-948: Fixed file permissions on Launcher install scripts for Linux

0.34.0-beta.7
------

### Fixed
- HUB-920: hub-cache - temporary fix for LRU updates being called on non-COMMITTED records

0.34.0-beta.6
------

### Fixed
- HUB-941: Partially reverted: Move hub binary back into "target/release/" folder for backwards compatibility.
- Hub client will search for Hub in both "target/release/" and "release/" for forwards compatibility.

0.34.0-beta.5
------

### Fixed
- HUB-941: Fix packaging inconsistencies:
  - Remove "target/" from hub binary prefix.
  - Don't include windows files in linux packages and vis-versa.
- Fixed missing post-install scripts from Hub packaging

0.34.0-beta.3
------

### Fixed
- HUB-906: Use Launcher's finalize hook to indicate new Hub version is ready to use
- HUB-937: Launcher tray script on Linux fails to get support bundle

0.34.0-beta.2
------

### Fixed
- HUB-906: Fix pre install cleanup of stale hub installations from Launcher

0.34.0-beta.1
------

### Fixed
- HUB-906: Fix missing Launcher post install scripts

0.34.0-beta.0
------

### Added
- HUB-826: Improve block cache logging and errors
- HUB-906: Fix Launcher uninstall on Windows (file in use)
- HUB-926: Fix garbage collection bug in which the current active file can be accidentally collected if empty
- HUB-930: Added icon (Windows only)
- HUB-931: Added hub_get_software_version
- OM-115938: Fixed spinning Launcher UI launch button

0.33.0-beta.2
------

### Fixed
- Hub version in binary

0.33.0-beta.1
------

### Fixed
- Launcher package creation

0.33.0-beta.0
------

### Fixed
- OM-114531: fix bug where preallocated files would improperly renamed when moved to active

0.32.0-beta.0
------

### Fixed
- HUB-905: Fixed multiple garbage collection bugs. Improved performance of garbage collection

0.31.0-beta.0
------

### Fixed
- HUB-900: Fix static destruction crash with hub client in client-library

0.30.0-beta.2
------

### Fixed
- Fix for Launcher UI crashing on missing Hub statistics fields

0.30.0-beta.1
------

### Fixed
- Package version parsing

0.30.0-beta.0
------
### Fixed
- HUB-812: Do not check if provided OMNICLIENT_HUB_CACHE_DIR path exists in Hub Client. The Hub server will create the cache
           path if it does not exist, so this check was unnecessary
- HUB-810: Add log file rotation. hub.toml options for count, max_size, compression

### Added
- HUB-690: Adds Block Cache functions to Hub
  - This change adds a new 'BlockCache' interface to the Hub API for storing blocks of data rather than entire files.
  - A new cache backend was implemented to efficiently store many blocks in the same file
  - The existing file cache was reimplemented using the new cache backend so that there's one unified cache
  - This change includes breaking changes to the existing CacheAside API. Older clients should update to the new Beta Hub Client
    If an older client connects, this server version will indiciate that it does not support the client, and the client will not
    issue any requests to the server, instead it will fail requests immediately indicating the client is not supported

### Updated

- HUB-862: Updated Hub launcher icon.
- HUB-840: Increase default cache size to 150GB

0.20.4-alpha.3
------

### Fixed
- Hotfix for starting Hub infinite loop when cache directory does not exist

0.20.4-alpha.2
------

### Added

- HUB-712: Implementation of Hub self-launching RFC
  - Hub can now launch in 2 modes: shared or exclusive
    - shared used for shared hub instance on workstations
    - exclusive mode for CI/CD, containers. Hub process tied to parent process, exits when parent exits
HUB-698: Swap underlying IPC from Tokio + Capnproto to Tokio + MessagePack. Older clients using capnproto still supported
HUB-703: Implement most of the Hub client in Rust and use `cbindgen` for C bindings to be used in Client Library
HUB-787: Revert HUB-730 until newer Launcher is released to production to support HUB-730

### Fixed

- Potential crash during client shutdown on Windows

0.20.3
------

### Added

- HUB-721: Update documentation publishing: get version from `CHANGELOG.md` and update links when publishing to launcher

0.20.1
------

### Added

- HUB-668: Added support bundle download button in the web interface

### Fixed

- HUB-662: Fixed opening web interface (settings), from tray icon.

0.10.60
------

### Fixed

- HUB-655: Fixed issues on the windows and linux with hub/port detection

0.10.59
------

### Fixed

- HUB-655: Fixed incorrect configuration parsing for omniverse.toml for cache folder

0.10.58
------

### Fixed

- HUB-645: Fixed issue with not showing web interface

0.10.57
------

### Fixed

- HUB-645: Launcher tries to open hub web interface on port 1

### Added

- HUB-621: Find solution to upgrading a running hub from launcher
- HUB-357: Updated hub description and added message.toml

0.10.56
------

### Added

- HUB-357: Add basic configuration for deploy to launcher

0.10.48
------

### Added

- HUB-434: Added web interface
- HUB-599: Updated description of Hub on launcher
- HUB-384: Add submenu to Omniverse tray icon
- HUB-190: RFCs
- HUB-414: Add API for collecting support bundles

0.10.47
------

### Added

- HUB-357: Hub workstation deployment
- HUB-434: Add API endpoint to update configuration

0.1.0
------

### Added

- HUB-579: Release 1.0 tasks
    - Manual publishing of pipelines for all branches except protected branches
    - Use full versions in Cargo.toml
    - Check in Cargo.lock
    - CHANGELOG.md for Hub and Hub Cache
- HUB-594: fix NSPECT BDSA-2021-4462 vulnerability with `rand_core` crate from deprecated `TempDir` crate
