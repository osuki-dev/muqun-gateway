# Desktop installation and local Gateway

## Install a published Desktop package

Use the official installer on Linux x86_64 or Apple silicon macOS. It reads the release manifest from muqun.dev and verifies the package size, SHA-256 digest and companion checksum before installation. macOS also verifies the app's code signature and Gatekeeper assessment. The full installer needs Python 3 and curl. The bundled Gateway-only entry point uses the system shell.

```sh
curl -fsSL https://muqun.dev/desktop.sh | sh  # Stable Desktop and a missing local Gateway
sh install-desktop.sh --channel preview       # Desktop preview and a missing preview Gateway
sh install-desktop.sh --tag desktop-vX.Y.Z    # Pin a published Desktop release
sh install-desktop.sh --without-gateway       # In-process agents or a paired remote computer
```

Choose an actual published version for `--tag`. If the requested channel has no Desktop release, installation stops before changing any installed package. Desktop and Gateway share a GitHub repository and have independent release identities.

| Component | Release tags | Platform packages |
| --- | --- | --- |
| Desktop | `desktop-vX.Y.Z` or `desktop-vX.Y.Z-preview.N` | Linux x86_64 tarball, macOS arm64 notarized app ZIP |
| Gateway | `vX.Y.Z` or `vX.Y.Z-preview.N` | Linux x86_64/arm64 and macOS x86_64/arm64 binaries |

A stable Desktop release starts when a maintainer pushes a reviewed `desktop-vX.Y.Z` tag matching the Desktop crate version and its committed `release-notes/desktop-vX.Y.Z.md`. CI builds both packages, runs the Linux smoke tests, notarizes the macOS app, verifies the package versions and checksums, and publishes and mirrors the release. To build a Desktop preview, manually run **preview** with product **desktop**. Set **publish_desktop** to publish the signed candidate. [Stable workflow](../.github/workflows/desktop-release.yml) · [Preview workflow](../.github/workflows/preview.yml)

## Where the files go

Linux installs the packaged executable and WebView helper under `~/.local/bin`, plus launcher, icons, bundled plugins and installers under `~/.local/share`. Set `PREFIX` for a different location. macOS installs `Muqun Desktop.app` into `~/Applications`. An existing copy in `/Applications` is updated there when writable. Set `MUQUN_DESKTOP_APPLICATIONS_DIR` to select another location.

Rerun the installer to update Desktop. It stages the package on the destination filesystem, serializes replacement and journals the files to restore if activation fails or is interrupted. Reopen Desktop to use the replacement. Settings → About checks the installed Desktop channel on muqun.dev and links to the installation guide. [Platform packaging](../desktop/scripts)

## Gateway provisioning and startup

The installer reuses an existing Gateway installation or service, including a stopped or externally managed installation. On a fresh computer it invokes the bundled official Gateway installer. A stable Desktop package provisions a stable Gateway. A preview Desktop package provisions a preview Gateway. Each Gateway binary is independently verified before setup. Setup requires an existing tmux or Herdr installation. Any persistent startup registration follows Gateway's own prompts and uses a user systemd unit on Linux or a LaunchAgent on macOS. Gateway setup errors leave the installed Desktop available. Resolve the reported prerequisite or installation error and retry. [Gateway installer](../install.sh) · [Standalone setup](../README.md)

The same Gateway-only entry point is bundled for direct app downloads. Desktop restores the selected paired computer first. On this computer it probes the Gateway's `local-client.json` and authenticated `GET /api/meta`. When the service is unavailable, the in-process backend supports local work without opening a network listener. For a fresh packaged install Desktop can provision the missing service in the background. Diagnostics shows its status, retry and connection actions. A running session can finish before the switch. `--without-gateway` persists the local-only preference in Desktop's config directory. An offline remote computer stays selected. [Desktop startup and preferences](../desktop/README.md)

## Updating or removing Gateway

Use the original installation owner for Gateway upgrades. A standalone Gateway's protected updater locks the executable and service lifecycle, installs the checked version and tests startup. Binary recovery does not undo database migrations. Preserve matching state, configuration and encryption keys when a migration requires recovery. [Gateway update and recovery](../README.md#manual-stable-updates)

On Linux, run the installed `share/muqun-desktop/installers/uninstall-local.sh` using the same `PREFIX`. On macOS remove the Desktop app bundle. Both leave Gateway, paired devices, user plugins and configuration available. To change Gateway startup separately, use `muqun-gateway service uninstall`. A running Gateway continues as an ordinary background process until explicitly stopped.

## Gateway and Desktop release mirror

CI mirrors published stable and preview releases for both Gateway and Desktop into the Cloudflare R2 bucket `muqun-themes`. The mirror also handles manually published releases, including stable Desktop tags. To backfill an existing tag or retry a failed mirror, run **Mirror published release** with its exact GitHub tag. The job requires the complete product-specific platform matrix, verifies package sizes and companion SHA-256 files, and checks GitHub's asset digests when present. Historical Gateway releases that contain only the four binaries can be backfilled if GitHub provides a SHA-256 for every binary. The mirror verifies those hashes and generates the standard checksum files.

Configure Actions secrets `CLOUDFLARE_ACCOUNT_ID`, `R2_ACCESS_KEY_ID`, and `R2_SECRET_ACCESS_KEY` with object read/write access to the existing bucket. Ordinary builds run offline tests.

| Bucket key | Contents |
| --- | --- |
| `gateway/releases/v<version>/` | Four Linux/macOS binaries, one `.sha256` file per binary and `manifest.json` |
| `desktop/releases/desktop-v<version>/` | Linux x86_64 tarball, notarized macOS arm64 ZIP, one `.sha256` file per package and `manifest.json` |
| `<product>/channels/stable.json`, `<product>/channels/preview.json` | Current version for this product and channel |

Each manifest records the channel, version, tag, platform, filename, byte size and SHA-256. The publisher reads back and hashes every immutable object before updating the channel. A retry reuses identical files. Conflicting bytes at an existing key fail publication. Conditional writes and numeric version ordering prevent concurrent releases and backfills from rolling a channel back. Clients read the channel at `https://muqun.dev/api/releases/<product>/<stable-or-preview>`, then fetch the manifest and files under `/api/releases/<product>/<tag>/`.


Successful Desktop CI builds on `main`, including manual runs on that branch, archive their candidates under `desktop/ci/<version>/<run-id>/<attempt>/`. Each run stores the two raw executables, Linux WebView helper, Linux and macOS packages, their checksums and the notarized macOS archive when signing runs. The run manifest lists the source commit and each artifact's relative path, platform, size and SHA-256. Installation channels advance only through release publication.
