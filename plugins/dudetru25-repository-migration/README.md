# Repository Migration plugin

This first-party local plugin re-points supported path-keyed state after a
repository moves on the same machine. It does not move a repository or retarget
a Herdr workspace.

Link the plugin from this repository:

```bash
herdr plugin link ./plugins/dudetru25-repository-migration
herdr plugin config-dir dudetru25.repository-migration
```

For a complete repository migration, create `migration.json` in the printed
config directory. It must name the old and new repository paths, the exact old
and new Claude project keys, the Claude projects directory, and each editor
workspace file to update:

```json
{
  "oldPath": "/old/location/repository",
  "newPath": "/new/location/repository",
  "claudeProjectsRoot": "/home/user/.claude/projects",
  "claudeRootQuiescent": true,
  "oldProjectKey": "-old-location-repository",
  "newProjectKey": "-new-location-repository-8f3e18c1",
  "editorWorkspaceFiles": [
    "/home/user/workspaces/repository.code-workspace"
  ]
}
```

The Claude keys must be the exact existing and intended directory basenames.
The plugin does not reproduce Claude's key algorithm because long paths can use
a version-specific hash suffix. It also does not scan the home directory or
search for workspace files. Before setting `claudeRootQuiescent` to `true`, stop
every Claude process that uses the named projects root and keep that root
quiescent for the complete action. The plugin refuses full migration without
this explicit acknowledgement.

Invoke the complete action:

```bash
herdr plugin action invoke dudetru25.repository-migration.migrate-repository-paths
```

The action:

- Updates only these supported values under the moved checkout's
  `.git/skills-v4` state:
  - `worktree`, `gitCommonDirectory`, and `gitDirectory` in
    `skills-ticket-placement/v1` records.
  - `worktree` in `skills-snapshot/v4` and `skills-snapshot/v5` records.
  - Values on lines that start exactly with `Claimed worktree: ` in tracker
    Markdown files.
- Renames the exact Claude project directory from `oldProjectKey` to
  `newProjectKey` inside `claudeProjectsRoot`.
- Updates only `folders[*].path` values in the named `.code-workspace` files.
  Absolute paths stay absolute and relative paths stay relative. URI entries,
  settings, and unrelated strings are not changed.

The earlier Skills-only action remains available. For that action,
`migration.json` must contain only `oldPath` and `newPath`:

```bash
herdr plugin action invoke dudetru25.repository-migration.migrate-skills-v4
```

The actions do not recursively replace arbitrary text. They reject filesystem
roots, overlapping old and new paths, Claude destination collisions, symbolic
links, malformed records, unknown schemas, concurrent content or mode changes,
and inputs that exceed fixed traversal, file, byte, depth, field, or report
limits. Each changed file is replaced atomically in its existing directory and
keeps its file mode. Under the validated quiescence contract, the Claude
directory is renamed atomically within its existing parent after an identity
recheck while a cooperative migration lock is held.

The action prints a compact JSON summary. It writes the complete report to
`migration-report.json` under `HERDR_PLUGIN_STATE_DIR`. The summary includes a
non-null `reportPath` only after that report is committed. If report delivery
fails, the bounded report is returned inline with `reportPath: null`, so a stale
file is never presented as the current result. If an abnormal concurrent
failure would exceed the output limit, the inline summary sets `truncated` to
true instead of emitting an unbounded stream. The process exits with status 0
only for a complete migration. A partial or failed migration exits with status
1 and lists every path it could not change in the report. Each change and
failure names its consumer, and the report includes a summary for Skills V4,
Claude projects, and editor workspaces.
