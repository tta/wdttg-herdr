# wdttg-herdr

Herdr plugin that auto-tracks time per workspace into [wdttg](../wdttg).

Each herdr workspace maps to a wdttg task (`herdr:<label>`). When you focus a
workspace, a session starts on its task. When you leave (to another workspace
or outside herdr), the session enters a *pending* state. If you return within
the bridge threshold (default 3 minutes), the session is extended across the
gap. If you don't return in time, the session closes at the moment you left and
the gap is untracked.

Reports stay in wdttg: `wdttg report`, `wdttg review`, `wdttg trend`.

## How it works

- A daemon (`target/release/wdttg-herdr`) polls `herdr api snapshot` every few
  seconds, reads the focused workspace, and drives a per-workspace session
  state machine.
- The daemon talks to wdttg directly via its library (path dependency), writing
  to the same SQLite database the wdttg CLI uses.
- A tab bar entry shows the current tracked task and elapsed time.

## Setup

### 1. Install wdttg

```sh
cd ~/repos/personal/wdttg
cargo install --path=. --root=${HOME}/.local/
```

### 2. Build this plugin

```sh
cd ~/repos/personal/wdttg-herdr
cargo build --release
```

### 3. Link the plugin in herdr

```sh
herdr plugin link ~/repos/personal/wdttg-herdr
```

The `[[build]]` command in `herdr-plugin.toml` runs `cargo build --release`;
the `[[startup]]` hook starts the daemon on each herdr session start.

### 4. Add the tab bar status entry

In your herdr config:

```toml
[ui]
tab_bar_right = [
  { type = "command", command = "sh ~/repos/personal/wdttg-herdr/scripts/status.sh", interval_seconds = 5, timeout_seconds = 2 },
]
```

The tab bar will show e.g. `⏱ herdr:wdttg 1h 23m` while tracking, `⏸ wdttg
paused` when paused, or `○ wdttg idle` when no workspace is focused.

## Configuration

Place a `config.toml` in the plugin's config directory
(`HERDR_PLUGIN_CONFIG_DIR`):

```toml
bridge_threshold_seconds = 180   # gap to bridge (default 3 min)
poll_interval_seconds = 3        # herdr poll interval (default 3s)
task_name_template = "herdr:{label}"  # task naming (default)
rename_timeout_seconds = 60      # rename prompt timeout (default 60s)
```

## Workspace rename

If you rename a herdr workspace that has an active or pending session, the
daemon detects the label change and prompts (via a `rename_decision.txt` file
in the state dir):

- `continue` (default) — renames the wdttg task, keeping all history.
- `new` — closes the old session and starts a fresh task.

Write your choice to `$HERDR_PLUGIN_STATE_DIR/rename_decision.txt` (`continue`
or `new`). If no decision is written within `rename_timeout_seconds`, the
daemon defaults to `continue`.

## Pause / resume

```sh
sh scripts/pause.sh    # stop tracking (daemon records focus but no sessions)
sh scripts/resume.sh   # resume tracking
```

These are also exposed as herdr plugin actions (`wdttg.timer.pause`,
`wdttg.timer.resume`).

## Plugin panes

The manifest declares three popup panes for quick access to wdttg reports:

- `wdttg.timer.timeline` — today's timeline
- `wdttg.timer.review` — weekly review
- `wdttg.timer.quadrant-report` — quadrant report for the week

Invoke via herdr's plugin pane mechanism (keybindings are deferred until the
workflow is familiar).

## Development

```sh
cargo build       # debug build
cargo test        # unit tests (tracker state machine)
cargo clippy      # lint
```

The tracker state machine (`src/tracker.rs`) is pure logic and fully unit
tested — focus/blur, bridge, threshold expiry, parallel workspaces, pause,
rename detection and resolution.

### Smoke test against a live herdr

```sh
WDTTG_DB=/tmp/test.db ./target/debug/wdttg-herdr
# focus a workspace, wait ~5s, then check:
WDTTG_DB=/tmp/test.db wdttg status
```
