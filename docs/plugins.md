# Plugins and marketplaces

The **Plugins** button beside **Output** in the status bar opens discovery and
plugin management. Browse the official marketplace and any custom public Git
marketplaces configured for your account. Installed and available plugins appear
in compact rows with publisher/version details and inline actions. Search filters
both sections. Available excludes installed plugins; choose their releases from
the Installed gear menu. Uninstalling returns a plugin to Available. Listings show their marketplace as a Source label, with the official
source named Open WebIDE. Click a row’s name to inspect plugin details
and use the gear menu to choose a release. Install it on the open project's
execution host, or on the server host when no project is open. Plugins currently
contribute agent skills and optional platform tool groups.

Execution hosts bundle a commit-pinned subset of the official marketplace: Web,
Project Memory, Scheduling and Skill Authoring. Account initialization installs
missing defaults once through the normal validated host installation flow, with
Notify updates. Existing accounts receive this baseline too. PR Review stays optional.
The database remembers installation/removal by plugin identity; later sign-ins,
refreshes and app upgrades do not reinstall removed defaults, reset project
opt-outs or replace a selected version. Host unavailability defers initialization;
sign-in and existing installations remain available, and refresh retries it.

The selection is locked in `plugins/bundled.json`. `tools/bundle_plugins.py`
materializes only those package directories from the pinned public Git commit,
along with its MIT license. Docker builds regenerate the host snapshots; CI
checks the committed snapshots against upstream. Native builds embed the checked-in
snapshots so development builds and first installation need no network. Browsers
receive manifests and managed skills through the existing APIs; plugin files are
prepared and published on execution hosts. Updates still use the marketplace.
To change the baseline, edit the lock, run `python3 tools/bundle_plugins.py`, and
commit the regenerated host snapshots. Use `--repository <upstream-clone>` to
regenerate or check without network access.

Installation enables a plugin across your existing projects by default. New
projects inherit installed plugins too. **Disable for project** saves an opt-out;
updates preserve it. **Enable for project** removes that opt-out by loading the
installed version. Verified instructions/resources are stored as managed project
skills, with visible provenance. Personal skills remain independent; a contributed
skill name collision aborts the installation transaction without overwriting data.
The project's global Skills switch still applies.

Use **Manage marketplace sources** in the Plugins hamburger menu to open
Settings → Plugins. That settings tab
only configures marketplace sources; search, install, uninstall and project
enable/disable controls live in the Plugins interface.

The official source is `https://github.com/openwebide/plugins.git`, using its
default branch and root `marketplace.json`. It is built in and cannot be removed.
Additional sources accept a public Git repository URL, optional branch/tag/commit
reference, and catalog file path.
Releases declare only their immutable commit and package directory; every package
inherits its marketplace's repository. Refreshing catalogs updates discovery; Notify installations remain pinned. A failed refresh preserves cached
releases and reports the failed source. Removing a custom source stops discovery
without uninstalling its packages. Sources and caches are user-scoped database settings.

The Plugins status-bar button shows a count when updates are available. The
Installed heading shows **Update all (N)** when updates are available, even
when the section is collapsed; each installed plugin's gear menu offers an
individual update, release selection and update preferences. **Notify** is the
default: checking a catalog does not install its newer releases. **Automatic**
applies compatible updates; major version changes, and minor changes before 1.0,
still need a manual update. **Off** hides update notifications and prevents automatic
updates. Checks run at sign-in and every 15 minutes while the app is open. Failed
checks preserve cached releases; failed updates preserve the installed version.

Selecting another release updates or rolls back the installation and enabled
projects together. Project opt-outs remain disabled. **Uninstall** removes the
installation, inherited defaults and managed skills from all your projects,
without deleting personal memories, skills or schedules. Host snapshots and active
run instructions/resources remain pinned; changes apply to subsequent runs.

For packages outside a catalog, choose **Install a pinned plugin manually** from
the Plugins hamburger menu and
enter a public Git repository URL (HTTP, HTTPS, SSH or Git protocol), a full
lowercase commit ID, and the directory containing `plugin.json`; use `.` for the
repository root. The reference PR Review 0.1.0 package uses:

- Repository: `https://github.com/openwebide/plugins.git`
- Commit: `e79185c2b25f713503b70e23ee7e91e66c5af208`
- Package directory: `plugins/pr-review`

Local projects need their paired native bridge and folder access. Remote projects
use the server bridge, including when opened from a phone. The browser does not
clone packages or execute plugin code. Fetching uses the bridge host's existing Git
credentials; credentials cannot be embedded in repository URLs.

The shared plugin facade selects the project host. Both transports use the same
core validation and installation policy. The native bridge reads Git objects from
a bare cache, validates API 1 skills and API 2 platform tool-group manifests and skill resources, and
publishes a commit/content-addressed snapshot by atomic rename. Hooks, checkout
filters, package scripts and runtime installers are not run. Symlinks, submodules,
path collisions and unsupported contributions are rejected. Packages are limited
to 2,048 files, 128 KiB per file and 16 MiB in total; contributed skills must also
meet the existing project skill import limits.

Caches live outside project folders. Native bridges use
`$XDG_DATA_HOME/openwebide/plugins` or `~/.local/share/openwebide/plugins`;
`OPENWEBIDE_PLUGIN_DIR` overrides that path with an absolute host cache path. The container uses
`/app/.spin/plugins` on the existing persistent volume. Backend users have separate
cache namespaces; locally paired clients use the paired host namespace.

Logical installation records are user-scoped database settings, shared across
clients. They include the source commit, validated manifest, content digest and
historical preparation receipts for each host. Receipts describe a completed
preparation, not ongoing host availability. Reinstalling on another host prepares
that same version there. Preparing the same cached version verifies it before
returning success and works offline. A modified snapshot is rejected rather than
silently repaired in place.

To change the selected commit, refresh the installation list and install the new
commit. Database updates check the installation revision, and preparation failures
leave the previous record and snapshot intact. Account, project, session or host
changes prevent stale browser results from recording an installation. An already
submitted server transaction may finish for its authenticated account.

Plugin API 2 adds `contributions.toolGroups`: `web`, `memory`, `scheduling` and
`skill-authoring`. A plugin can combine these with skills or contribute a tool
group alone with `skills: []`. These groups use the existing shared platform
handlers and approval rules. File editing, shell execution, git, questions and
task coordination remain core tools. The first-party Skill Authoring plugin carries the authoring instructions; its
tool handlers still live in the app. Discovery/read tools remain available.
Both run adapters apply one contribution policy, and scheduled host adaptation
preserves the selected tool set.

These tool-group manifests are transitional feature switches, not completed
migrations of executable behavior. The pinned default subset is Web, Project
Memory, Scheduling and Skill Authoring; PR Review remains optional.

## Executable plugin requirements

First-party plugins must own their behavior and use the same public Rust SDK,
versioned host interfaces and lifecycle as community plugins. They may use general
host capabilities such as workspace access, namespaced persistence, HTTP,
approved process execution and durable job dispatch. Feature-specific policy,
orchestration and result shaping must live in the plugin; a built-in feature
hidden behind a host API does not satisfy this requirement. There must be no
first-party identity dispatch, privileged feature endpoint or built-in fallback
when a plugin is missing, disabled or fails. Skill-only contributions may remain
instructions/resources without an executable.

The executable path ships Rust source and a committed Cargo.lock. API 3 declares
the library, SDK version, capabilities, exported tool schemas and event names. Installation
compiles source in isolation on the execution host, against its pinned compiler
and embedded public SDK, then validates the exported schemas before activation.
The artifact cache includes source, SDK and toolchain fingerprints. Compilation
failures preserve the installed version; clients never compile or execute plugins.
Publisher CI should validate source builds, but installing a plugin does not
require a publisher-hosted WASM artifact. Bundled plugins receive the same
interfaces and privileges.

The SDK, source preparation and shared execution workflow are foundations in
progress. HTTP and clock primitives run on the bridge. Private record callbacks
use opaque run grants checked against the authenticated account, session, original
project and pinned plugin source/capabilities. Record collections persist across
plugin updates and reinstalls, with revision checks, bounded pages and quotas.
Grant tokens stay in host orchestration and are never passed to plugin code.
Execution contexts can scope a grant to an owned project without a chat session.
They use the same storage callbacks and retain the selected primary model where
configured; chat planners pin their selected model, including run overrides.
Context grants and chat grants cannot be used through each other's callback
endpoint. Existing active grants retain their pinned plugin version across
updates or removal; new grants require the currently enabled installed version.
Memory UI mutations use the same declared tool invocation policy as agent runs,
with sessionless grants and no built-in behavior fallback. The explicit UI action
grant permits manual editing while automatic Memory context is switched off;
plugin code cannot opt itself into this authority. Plugins on a paired host do
not require that host to see a local project's browser folder. Workspace commands
and Git retain their separate folder mapping requirement. Durable background
delivery and executable-default deployment remain unfinished.
The separate `completion` grant exposes bounded text generation through the
session's configured primary or fast model. Plugins supply prompts and interpret
the results; the host supplies model selection and credentials. Inputs are limited
to 32 KiB, outputs to 1–1024 tokens and 16 KiB of text, with a 30-second completion
deadline and no tools. Context hooks cannot request completions. The source
Memory plugin owns its naming prompt, profile fallback and content-derived title.
Further shared collection adapters, job and workspace callbacks, installation progress
and cancellation, and compiled offline defaults still need implementation. API 2
first-party plugins remain transitional.

The separate `collections` capability exposes schema-validated CRUD for the
app-visible `memories` and `skills` collections. It preserves existing UI records and enforces
the user's Memory switch, ownership, quotas and revision checks; it does not
provide search, naming or context formatting. Private `records` grants do not
authorize these shared collections. The SDK also exports a read-only context
hook with an 8 KiB maximum contribution and the ability to disable only its own
declared tools. Shared run preparation executes that hook on both execution hosts
before model tool selection, including when model tools are disabled.

Plugins can also declare `contributions.events` and export matching names through
`Plugin::events()`. The host invokes `Plugin::event(EventInput)` through the same
capability-gated workflow as tools, using a fresh WASM instance. Event payloads
are bounded to 256 KiB; undeclared names are rejected before execution. An
event-only plugin may export no model-facing tools. These callbacks provide the
execution contract; durable queues, job delivery and scheduling remain unfinished.

Skills collection writes accept a `draft` object using the existing skill schema.
Reads include that draft and read-only `origin` metadata for managed plugin skills.
The host protects managed or disabled skills from mutation and omits disabled
skills from reads. Pages hold at most eight skills and approximately 1 MiB of
serialized records to bound resource transfers. Collection mutations support the
existing 128 KiB skill data schema even when JSON escaping expands its payload;
private records retain their 64 KiB value limit.
Authoring, search and prompt policy stay in plugin source.

Every shared tool, context and event invocation prepares the selected source on
its execution host before starting WASM. The host receipt must match the pinned
source, manifest and digest; a different host ID is allowed. Preparation failures
stop execution. Valid source and artifact caches support offline reuse.

Both host adapters prepare plugin tools through the shared run planner before
applying connection tool selection and model tool settings. A plugin without an
advertised tool retains no tool execution grant after context planning. Context
hooks receive temporary pinned authority independently of tool selection. The installed version remains active
when a prepared update requests additional capabilities, including compatible
automatic updates. The Plugins interface shows the additions for explicit review;
approval applies only to that prepared version and installation revision.

Source builds use a read-only source/SDK/toolchain and a separate writable work
directory. Dependency retrieval is isolated too, with network access enabled only
for retrieval. macOS uses its native sandbox; native Linux users use bubblewrap.
Root Linux containers use the packaged `openwebide-plugin-build` launcher with a
fresh unprivileged identity, fully enforced Landlock ABI 3, and a syscall filter.
The ordinary Docker profile stays in place. Container kernels must support that
Landlock ABI; compilation fails if the protections cannot be applied. The Docker
image supplies the pinned toolchain in a location the isolated identity can read.
The source-to-WASM/runtime contract runs in CI on macOS and in a default Linux
container, including attempts to read host files, alter source metadata and use
the network during compilation.

Web's source reference implementation now lives in the plugins repository and
uses only the public SDK HTTP primitive. Its API 3 release remains unlisted while
installation and migration verification continue. Memory's source implementation
now owns search, CRUD formatting and bounded context selection over the public
collection primitive; it remains unlisted pending planning and lifecycle
integration. Continue Scheduling and Skill Authoring. Verify the same behavior in
local and remote modes, including cancellation, crashes, disablement and updates,
before calling those migrations complete. Completion of executable plugins, MCP
servers, dependencies, language, UI and editor contributions remains on the roadmap.
