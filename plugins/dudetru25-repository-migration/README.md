# Repository Migration plugin

This first-party local plugin re-points supported Skills V4 state after a
repository moves on the same machine. It does not move a repository or retarget
a Herdr workspace.

Link the plugin from this repository:

```bash
herdr plugin link ./plugins/dudetru25-repository-migration
herdr plugin config-dir dudetru25.repository-migration
```

Create `migration.json` in the printed config directory. The file must contain
only two normalized absolute paths:

```json
{
  "oldPath": "/old/location/repository",
  "newPath": "/new/location/repository"
}
```

Invoke the action:

```bash
herdr plugin action invoke dudetru25.repository-migration.migrate-skills-v4
```

The action updates only these supported values under the moved checkout's
`.git/skills-v4` state:

- `worktree`, `gitCommonDirectory`, and `gitDirectory` in
  `skills-ticket-placement/v1` records.
- `worktree` in `skills-snapshot/v4` and `skills-snapshot/v5` records.
- Values on lines that start exactly with `Claimed worktree: ` in tracker
  Markdown files.

The action does not recursively replace arbitrary text. It rejects filesystem
roots, overlapping old and new paths, symbolic links, malformed records,
unknown schemas, concurrent content or mode changes, and inputs that exceed
its fixed traversal, file, byte, depth, or report limits. Each changed file is
replaced atomically in its existing directory and keeps its file mode.

The action prints a compact JSON summary. It writes the complete report to
`migration-report.json` under `HERDR_PLUGIN_STATE_DIR`. The summary includes a
non-null `reportPath` only after that report is committed. If report delivery
fails, the bounded report is returned inline with `reportPath: null`, so a
stale file is never presented as the current result. If an abnormal concurrent
failure would exceed the output limit, the inline summary sets `truncated` to
true instead of emitting an unbounded stream. The process exits with
status 0 only for a complete migration. A partial or failed migration exits
with status 1 and lists every path it could not change in the report.
