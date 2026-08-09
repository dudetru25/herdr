import assert from "node:assert/strict";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import {
  compactMigrationSummary,
  migrationModeForArguments,
  migrateRepositoryPaths,
  migrateSkillsV4,
  replacePathBoundary,
} from "../src/migrate.mjs";

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "herdr-repository-migration-"));
  t.after(() => rm(root, { recursive: true, force: true }));

  const oldPath = path.join(root, "old", "repo");
  const newPath = path.join(root, "new", "repo");
  const skillsRoot = path.join(newPath, ".git", "skills-v4");
  const configDir = path.join(root, "config");
  const stateDir = path.join(root, "state");
  await mkdir(path.join(skillsRoot, "placements"), { recursive: true });
  await mkdir(path.join(skillsRoot, "snapshots"), { recursive: true });
  await mkdir(path.join(skillsRoot, "backlog", "backlog", "tasks"), { recursive: true });
  await mkdir(configDir, { recursive: true });
  await mkdir(stateDir, { recursive: true });
  await writeFile(
    path.join(configDir, "migration.json"),
    `${JSON.stringify({ oldPath, newPath }, null, 2)}\n`,
  );

  return { configDir, newPath, oldPath, root, skillsRoot, stateDir };
}

async function writeJson(file, value) {
  await writeFile(file, `${JSON.stringify(value, null, 2)}\n`);
}

async function repositoryFixture(t) {
  const f = await fixture(t);
  const claudeProjectsRoot = path.join(f.root, "claude-projects");
  const oldProjectKey = "-old-location-repository";
  const newProjectKey = "-new-location-repository-a1b2c3d4";
  const oldClaudeProject = path.join(claudeProjectsRoot, oldProjectKey);
  const newClaudeProject = path.join(claudeProjectsRoot, newProjectKey);
  const absoluteWorkspace = path.join(f.root, "editor", "absolute.code-workspace");
  const relativeWorkspace = path.join(f.root, "editor", "relative.code-workspace");

  await mkdir(oldClaudeProject, { recursive: true });
  await writeFile(path.join(oldClaudeProject, "session.jsonl"), "live claude marker\n");
  await mkdir(path.dirname(absoluteWorkspace), { recursive: true });
  await writeJson(absoluteWorkspace, {
    folders: [
      { path: path.join(f.oldPath, "app") },
      { path: `${f.oldPath}-copy` },
      { uri: "vscode-remote://ssh-remote+dev/workspace" },
    ],
    settings: { "migration.unrelated": f.oldPath },
  });
  await writeJson(relativeWorkspace, {
    folders: [
      { path: path.relative(path.dirname(relativeWorkspace), path.join(f.oldPath, "docs")) },
    ],
  });
  await writeJson(path.join(f.configDir, "migration.json"), {
    oldPath: f.oldPath,
    newPath: f.newPath,
    claudeProjectsRoot,
    claudeRootQuiescent: true,
    oldProjectKey,
    newProjectKey,
    editorWorkspaceFiles: [absoluteWorkspace, relativeWorkspace],
  });
  await writeJson(path.join(f.skillsRoot, "placements", "TASK-DEFAULT.json"), {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "default"),
  });

  return {
    ...f,
    absoluteWorkspace,
    claudeProjectsRoot,
    newClaudeProject,
    newProjectKey,
    oldClaudeProject,
    oldProjectKey,
    relativeWorkspace,
  };
}

test("re-points Skills, explicit Claude project keys, and editor workspace paths", async (t) => {
  const f = await repositoryFixture(t);
  const placement = path.join(f.skillsRoot, "placements", "TASK-1.json");
  await writeJson(placement, {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "success");
  assert.deepEqual(
    new Set(report.changed.map(({ consumer }) => consumer)),
    new Set(["skills-v4", "claude-projects", "editor-workspaces"]),
  );
  assert.equal(await readFile(path.join(f.newClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
  await assert.rejects(lstat(f.oldClaudeProject), { code: "ENOENT" });

  const absolute = JSON.parse(await readFile(f.absoluteWorkspace, "utf8"));
  assert.equal(absolute.folders[0].path, path.join(f.newPath, "app"));
  assert.equal(absolute.folders[1].path, `${f.oldPath}-copy`);
  assert.equal(absolute.folders[2].uri, "vscode-remote://ssh-remote+dev/workspace");
  assert.equal(absolute.settings["migration.unrelated"], f.oldPath);

  const relative = JSON.parse(await readFile(f.relativeWorkspace, "utf8"));
  assert.equal(
    relative.folders[0].path,
    path.relative(path.dirname(f.relativeWorkspace), path.join(f.newPath, "docs")),
  );
});

test("reports Claude destination collisions without merging or overwriting", async (t) => {
  const f = await repositoryFixture(t);
  await mkdir(f.newClaudeProject);
  await writeFile(path.join(f.newClaudeProject, "existing.jsonl"), "keep target\n");

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "partial");
  assert.ok(
    report.failures.some(
      ({ consumer, code }) => consumer === "claude-projects" && code === "destination_exists",
    ),
  );
  assert.equal(await readFile(path.join(f.oldClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
  assert.equal(await readFile(path.join(f.newClaudeProject, "existing.jsonl"), "utf8"), "keep target\n");
  assert.ok(!report.changed.some(({ consumer }) => consumer === "claude-projects"));
});

test("reports a named partial result when an editor workspace is malformed", async (t) => {
  const f = await repositoryFixture(t);
  await writeFile(f.absoluteWorkspace, "{ // JSONC is not accepted\n}\n");

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "partial");
  assert.ok(report.changed.some(({ consumer }) => consumer === "claude-projects"));
  assert.ok(
    report.failures.some(
      ({ consumer, code, path: failedPath }) =>
        consumer === "editor-workspaces" &&
        code === "malformed_workspace" &&
        failedPath === f.absoluteWorkspace,
    ),
  );
  assert.equal(await readFile(path.join(f.newClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
});

test("rejects symlinked Claude projects and editor workspace files", async (t) => {
  const f = await repositoryFixture(t);
  const outsideClaude = path.join(f.root, "outside-claude");
  const outsideWorkspace = path.join(f.root, "outside.code-workspace");
  await mkdir(outsideClaude);
  await writeFile(path.join(outsideClaude, "marker"), "outside\n");
  await rm(f.oldClaudeProject, { recursive: true });
  await symlink(outsideClaude, f.oldClaudeProject);
  await writeJson(outsideWorkspace, { folders: [{ path: f.oldPath }] });
  await rm(f.absoluteWorkspace);
  await symlink(outsideWorkspace, f.absoluteWorkspace);

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "partial");
  assert.ok(
    report.failures.some(
      ({ consumer, code }) => consumer === "claude-projects" && code === "unsafe_file_type",
    ),
  );
  assert.ok(
    report.failures.some(
      ({ consumer, code }) => consumer === "editor-workspaces" && code === "unsafe_file_type",
    ),
  );
  assert.equal(JSON.parse(await readFile(outsideWorkspace, "utf8")).folders[0].path, f.oldPath);
  assert.equal(await readFile(path.join(outsideClaude, "marker"), "utf8"), "outside\n");
});

test("fails closed when only the Claude destination exists", async (t) => {
  const f = await repositoryFixture(t);
  await rename(f.oldClaudeProject, f.newClaudeProject);

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "partial");
  assert.deepEqual(report.consumers["claude-projects"], {
    status: "failed",
    changedFiles: 0,
    changedValues: 0,
    failures: 1,
  });
  assert.ok(
    report.failures.some(
      ({ consumer, code }) =>
        consumer === "claude-projects" && code === "unverifiable_destination",
    ),
  );
  assert.ok(!report.changed.some(({ consumer }) => consumer === "claude-projects"));
});

test("blocks every mutation when a full migration exceeds a shared resource limit", async (t) => {
  const f = await repositoryFixture(t);
  const placement = path.join(f.skillsRoot, "placements", "TASK-1.json");
  await writeJson(placement, {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
    limits: { maxFiles: 2 },
  });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "resource_limit"));
  assert.equal(JSON.parse(await readFile(placement, "utf8")).worktree, f.oldPath);
  assert.equal(await readFile(path.join(f.oldClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
  await assert.rejects(lstat(f.newClaudeProject), { code: "ENOENT" });
});

test("rejects unsafe Claude keys and duplicate editor workspace files", async (t) => {
  const f = await repositoryFixture(t);
  const baseConfig = JSON.parse(
    await readFile(path.join(f.configDir, "migration.json"), "utf8"),
  );
  const cases = [
    { ...baseConfig, oldProjectKey: "../outside" },
    {
      ...baseConfig,
      editorWorkspaceFiles: [f.absoluteWorkspace, f.absoluteWorkspace],
    },
    { ...baseConfig, unexpected: true },
  ];

  for (const config of cases) {
    await writeJson(path.join(f.configDir, "migration.json"), config);
    const report = await migrateRepositoryPaths({
      configDir: f.configDir,
      stateDir: f.stateDir,
    });
    assert.equal(report.status, "failed");
    assert.equal(report.failures[0].code, "invalid_config");
    assert.equal(await readFile(path.join(f.oldClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
    await assert.rejects(lstat(f.newClaudeProject), { code: "ENOENT" });
  }
});

test("requires an explicit true Claude-root quiescence acknowledgement", async (t) => {
  const f = await repositoryFixture(t);
  const configFile = path.join(f.configDir, "migration.json");
  const baseConfig = JSON.parse(await readFile(configFile, "utf8"));
  const { claudeRootQuiescent: _acknowledgement, ...missingAcknowledgement } = baseConfig;

  for (const config of [
    missingAcknowledgement,
    { ...baseConfig, claudeRootQuiescent: false },
  ]) {
    await writeJson(configFile, config);
    const report = await migrateRepositoryPaths({
      configDir: f.configDir,
      stateDir: f.stateDir,
    });

    assert.equal(report.status, "failed");
    assert.equal(report.failures[0].code, "invalid_config");
    assert.equal(await readFile(path.join(f.oldClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
    await assert.rejects(lstat(f.newClaudeProject), { code: "ENOENT" });
  }
});

test("rolls back a Claude rename when verification cannot complete", async (t) => {
  const f = await repositoryFixture(t);

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
    testHooks: {
      afterClaudeRenameBeforeVerify: async () => {
        throw new Error("simulated post-rename verification failure");
      },
    },
  });

  assert.equal(report.status, "partial");
  assert.ok(
    report.failures.some(
      ({ consumer, code }) =>
        consumer === "claude-projects" && code === "migration_failed",
    ),
  );
  assert.equal(await readFile(path.join(f.oldClaudeProject, "session.jsonl"), "utf8"), "live claude marker\n");
  await assert.rejects(lstat(f.newClaudeProject), { code: "ENOENT" });
  assert.ok(!report.changed.some(({ consumer }) => consumer === "claude-projects"));
});

test("does not report an editor change when its atomic replacement loses a race", async (t) => {
  const f = await repositoryFixture(t);

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
    testHooks: {
      beforeReplace: async (file) => {
        if (file === f.absoluteWorkspace) {
          await chmod(file, 0o600);
        }
      },
    },
  });

  assert.equal(report.status, "partial");
  assert.ok(
    report.failures.some(
      ({ consumer, code, path: failedPath }) =>
        consumer === "editor-workspaces" &&
        code === "concurrent_change" &&
        failedPath === f.absoluteWorkspace,
    ),
  );
  assert.ok(
    !report.changed.some(
      ({ consumer, path: changedPath }) =>
        consumer === "editor-workspaces" && changedPath === f.absoluteWorkspace,
    ),
  );
  assert.equal(
    JSON.parse(await readFile(f.absoluteWorkspace, "utf8")).folders[0].path,
    path.join(f.oldPath, "app"),
  );
});

test("reports an unrelated configured workspace instead of calling it unchanged", async (t) => {
  const f = await repositoryFixture(t);
  await writeJson(f.absoluteWorkspace, {
    folders: [{ path: path.join(f.root, "unrelated") }],
  });
  await writeJson(f.relativeWorkspace, {
    folders: [{ uri: "vscode-remote://ssh-remote+dev/unrelated" }],
  });

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "partial");
  assert.equal(report.consumers["editor-workspaces"].status, "failed");
  assert.equal(report.consumers["editor-workspaces"].failures, 2);
  assert.ok(
    report.failures.every(
      ({ consumer, code }) => consumer !== "editor-workspaces" || code === "no_matching_paths",
    ),
  );
});

test("accepts dot-relative workspace paths and keeps the replacement relative", async (t) => {
  const f = await repositoryFixture(t);
  const dotRelative = `.${path.sep}${path.relative(
    path.dirname(f.absoluteWorkspace),
    path.join(f.oldPath, "portable"),
  )}`;
  await writeJson(f.absoluteWorkspace, { folders: [{ path: dotRelative }] });

  const report = await migrateRepositoryPaths({
    configDir: f.configDir,
    stateDir: f.stateDir,
  });

  assert.equal(report.status, "success");
  const workspace = JSON.parse(await readFile(f.absoluteWorkspace, "utf8"));
  assert.equal(
    workspace.folders[0].path,
    path.relative(path.dirname(f.absoluteWorkspace), path.join(f.newPath, "portable")),
  );
});

test("keeps no-argument CLI compatibility and rejects unknown modes", () => {
  assert.equal(migrationModeForArguments([]), "skills-v4-only");
  assert.equal(migrationModeForArguments(["--skills-v4-only"]), "skills-v4-only");
  assert.equal(migrationModeForArguments(["--all-consumers"]), "all-consumers");
  assert.throws(
    () => migrationModeForArguments(["--unknown"]),
    ({ migrationCode }) => migrationCode === "invalid_arguments",
  );
});

test("accepts absolute Windows workspace paths with forward slashes", () => {
  assert.equal(
    replacePathBoundary(
      "C:/old/repository/app",
      "C:\\old\\repository",
      "D:\\new\\repository",
    ),
    "D:/new/repository/app",
  );
  assert.equal(
    replacePathBoundary(
      "C:/old/repository-copy/app",
      "C:\\old\\repository",
      "D:\\new\\repository",
    ),
    null,
  );
});

test("re-points only supported Skills V4 path fields and exact tracker notes", async (t) => {
  const f = await fixture(t);
  const placement = path.join(f.skillsRoot, "placements", "TASK-1.json");
  const snapshotV4 = path.join(f.skillsRoot, "snapshots", "TASK-1.json");
  const snapshotV5 = path.join(f.skillsRoot, "snapshots", "TASK-2.json");
  const ticket = path.join(f.skillsRoot, "backlog", "backlog", "tasks", "task-1.md");
  const unrelated = path.join(f.root, "elsewhere");
  const siblingPrefix = `${f.oldPath}-copy`;

  await writeJson(placement, {
    schema: "skills-ticket-placement/v1",
    worktree: path.join(f.oldPath, "worktree"),
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "worktree"),
    note: `do not rewrite ${f.oldPath} outside accepted fields`,
  });
  await chmod(placement, 0o640);
  await writeJson(snapshotV4, {
    schema: "skills-snapshot/v4",
    worktree: f.oldPath,
    nextAction: `leave ${f.oldPath} unchanged here`,
  });
  await writeJson(snapshotV5, {
    schema: "skills-snapshot/v5",
    worktree: siblingPrefix,
  });
  await writeFile(
    ticket,
    [
      "Claimed worktree: " + path.join(f.oldPath, "worktree"),
      "Other text: " + f.oldPath,
      "  Claimed worktree: " + f.oldPath,
      "Claimed worktree: " + unrelated,
      "",
    ].join("\n"),
  );

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "success");
  assert.equal(report.summary.changedFiles, 3);
  assert.equal(report.summary.changedValues, 5);
  assert.equal(report.summary.failures, 0);

  const migratedPlacement = JSON.parse(await readFile(placement, "utf8"));
  assert.equal(migratedPlacement.worktree, path.join(f.newPath, "worktree"));
  assert.equal(migratedPlacement.gitCommonDirectory, path.join(f.newPath, ".git"));
  assert.equal(
    migratedPlacement.gitDirectory,
    path.join(f.newPath, ".git", "worktrees", "worktree"),
  );
  assert.equal(migratedPlacement.note, `do not rewrite ${f.oldPath} outside accepted fields`);
  assert.equal((await lstat(placement)).mode & 0o777, 0o640);

  const migratedV4 = JSON.parse(await readFile(snapshotV4, "utf8"));
  assert.equal(migratedV4.worktree, f.newPath);
  assert.equal(migratedV4.nextAction, `leave ${f.oldPath} unchanged here`);
  assert.equal(JSON.parse(await readFile(snapshotV5, "utf8")).worktree, siblingPrefix);

  const migratedTicket = await readFile(ticket, "utf8");
  assert.match(migratedTicket, new RegExp(`^Claimed worktree: ${escapeRegExp(path.join(f.newPath, "worktree"))}$`, "m"));
  assert.match(migratedTicket, new RegExp(`^Other text: ${escapeRegExp(f.oldPath)}$`, "m"));
  assert.match(migratedTicket, new RegExp(`^  Claimed worktree: ${escapeRegExp(f.oldPath)}$`, "m"));

  const savedReport = JSON.parse(
    await readFile(path.join(f.stateDir, "migration-report.json"), "utf8"),
  );
  assert.deepEqual(savedReport, report);
  assert.deepEqual(
    report.changed.map(({ path: changedPath }) => changedPath),
    [
      "backlog/backlog/tasks/task-1.md",
      "placements/TASK-1.json",
      "snapshots/TASK-1.json",
    ],
  );
});

test("reports a partial migration when a supported change and an unknown schema coexist", async (t) => {
  const f = await fixture(t);
  await writeJson(path.join(f.skillsRoot, "placements", "TASK-1.json"), {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });
  await writeJson(path.join(f.skillsRoot, "snapshots", "TASK-bad.json"), {
    schema: "skills-snapshot/v6",
    worktree: f.oldPath,
  });

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "partial");
  assert.equal(report.summary.changedFiles, 1);
  assert.equal(report.summary.failures, 1);
  assert.deepEqual(report.failures[0], {
    path: "snapshots/TASK-bad.json",
    code: "unsupported_schema",
    detail: "unsupported snapshot schema: skills-snapshot/v6",
  });
});

test("rejects symlinked state files instead of following them", async (t) => {
  const f = await fixture(t);
  const outside = path.join(f.root, "outside.json");
  await writeJson(outside, {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });
  await symlink(outside, path.join(f.skillsRoot, "placements", "TASK-link.json"));

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "unsafe_file_type"));
  assert.equal(JSON.parse(await readFile(outside, "utf8")).worktree, f.oldPath);
});

test("rejects a symlinked state directory instead of traversing it", async (t) => {
  const f = await fixture(t);
  const outside = path.join(f.root, "outside-placements");
  const placements = path.join(f.skillsRoot, "placements");
  await mkdir(outside);
  await rm(placements, { recursive: true });
  await symlink(outside, placements);
  await writeJson(path.join(outside, "TASK-1.json"), {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "unsafe_file_type"));
  assert.equal(JSON.parse(await readFile(path.join(outside, "TASK-1.json"), "utf8")).worktree, f.oldPath);
});

test("fails explicitly when no supported value uses the old path", async (t) => {
  const f = await fixture(t);
  await writeJson(path.join(f.skillsRoot, "snapshots", "TASK-1.json"), {
    schema: "skills-snapshot/v5",
    worktree: path.join(f.root, "unrelated"),
  });

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "failed");
  assert.equal(report.failures.at(-1).code, "no_matching_paths");
});

test("requires migration.json to contain only normalized absolute oldPath and newPath", async (t) => {
  const f = await fixture(t);
  await writeJson(path.join(f.configDir, "migration.json"), {
    oldPath: f.oldPath,
    newPath: f.newPath,
    recursiveReplace: true,
  });

  const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });

  assert.equal(report.status, "failed");
  assert.equal(report.failures[0].code, "invalid_config");
  assert.equal(report.summary.changedFiles, 0);
});

test("rejects filesystem roots and overlapping migration paths", async (t) => {
  const f = await fixture(t);
  const cases = [
    { oldPath: path.parse(f.oldPath).root, newPath: f.newPath },
    { oldPath: f.oldPath, newPath: path.join(f.oldPath, "moved") },
    { oldPath: path.join(f.newPath, "nested"), newPath: f.newPath },
  ];

  for (const migration of cases) {
    await writeJson(path.join(f.configDir, "migration.json"), migration);
    const report = await migrateSkillsV4({ configDir: f.configDir, stateDir: f.stateDir });
    assert.equal(report.status, "failed");
    assert.equal(report.failures[0].code, "invalid_config");
    assert.equal(report.summary.changedFiles, 0);
  }
});

test("rejects a concurrent mode change before atomic replacement", async (t) => {
  const f = await fixture(t);
  const placement = path.join(f.skillsRoot, "placements", "TASK-1.json");
  await writeJson(placement, {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });
  await chmod(placement, 0o640);

  const report = await migrateSkillsV4({
    configDir: f.configDir,
    stateDir: f.stateDir,
    testHooks: {
      beforeReplace: async (file) => {
        if (file === placement) {
          await chmod(file, 0o600);
        }
      },
    },
  });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "concurrent_change"));
  assert.equal(JSON.parse(await readFile(placement, "utf8")).worktree, f.oldPath);
  assert.equal((await lstat(placement)).mode & 0o777, 0o600);
});

test("does not point to a stale report when report delivery fails", async (t) => {
  const f = await fixture(t);
  const placement = path.join(f.skillsRoot, "placements", "TASK-1.json");
  const reportFile = path.join(f.stateDir, "migration-report.json");
  await writeJson(placement, {
    schema: "skills-ticket-placement/v1",
    worktree: f.oldPath,
    gitCommonDirectory: path.join(f.oldPath, ".git"),
    gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
  });
  await writeFile(reportFile, "stale report\n");

  const report = await migrateSkillsV4({
    configDir: f.configDir,
    stateDir: f.stateDir,
    testHooks: {
      beforeWriteReport: async () => {
        throw new Error("simulated report failure");
      },
    },
  });
  const summary = compactMigrationSummary(report);

  assert.equal(report.status, "partial");
  assert.deepEqual(report.delivery, { status: "failed", reportPath: null });
  assert.equal(summary.reportPath, null);
  assert.equal(summary.reportDelivery, "failed");
  assert.ok(summary.failures.some(({ code }) => code === "report_write_failed"));
  assert.ok(Buffer.byteLength(`${JSON.stringify(summary)}\n`) <= 48 * 1024);
  assert.equal(await readFile(reportFile, "utf8"), "stale report\n");
});

test("fails before mutation when migration input limits are exceeded", async (t) => {
  const f = await fixture(t);
  const placements = ["TASK-1.json", "TASK-2.json"].map((name) =>
    path.join(f.skillsRoot, "placements", name),
  );
  for (const placement of placements) {
    await writeJson(placement, {
      schema: "skills-ticket-placement/v1",
      worktree: f.oldPath,
      gitCommonDirectory: path.join(f.oldPath, ".git"),
      gitDirectory: path.join(f.oldPath, ".git", "worktrees", "one"),
    });
  }

  const report = await migrateSkillsV4({
    configDir: f.configDir,
    stateDir: f.stateDir,
    limits: { maxFiles: 1 },
  });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "resource_limit"));
  for (const placement of placements) {
    assert.equal(JSON.parse(await readFile(placement, "utf8")).worktree, f.oldPath);
  }
});

test("rejects an oversized exact report before changing tracker state", async (t) => {
  const f = await fixture(t);
  const ticket = path.join(f.skillsRoot, "backlog", "backlog", "tasks", "task-many.md");
  const original = `${Array.from(
    { length: 300 },
    (_, index) => `Claimed worktree: ${path.join(f.oldPath, `worktree-${index}`)}`,
  ).join("\n")}\n`;
  await writeFile(ticket, original);

  const report = await migrateSkillsV4({
    configDir: f.configDir,
    stateDir: f.stateDir,
    limits: { maxReportBytes: 8 * 1024 },
  });

  assert.equal(report.status, "failed");
  assert.ok(report.failures.some(({ code }) => code === "resource_limit"));
  assert.equal(await readFile(ticket, "utf8"), original);
});

test("bounds migration config and git pointer reads", async (t) => {
  const configFixture = await fixture(t);
  await writeFile(path.join(configFixture.configDir, "migration.json"), "x".repeat(300));
  const configReport = await migrateSkillsV4({
    configDir: configFixture.configDir,
    stateDir: configFixture.stateDir,
    limits: { maxFileBytes: 256 },
  });
  assert.equal(configReport.status, "failed");
  assert.equal(configReport.failures[0].code, "resource_limit");

  const gitFixture = await fixture(t);
  await rm(path.join(gitFixture.newPath, ".git"), { recursive: true });
  await writeFile(path.join(gitFixture.newPath, ".git"), `gitdir: ${"x".repeat(1024)}\n`);
  const gitReport = await migrateSkillsV4({
    configDir: gitFixture.configDir,
    stateDir: gitFixture.stateDir,
    limits: { maxFileBytes: 512 },
  });
  assert.equal(gitReport.status, "failed");
  assert.equal(gitReport.failures[0].code, "resource_limit");

  const commonFixture = await fixture(t);
  const admin = path.join(commonFixture.root, "admin");
  await rm(path.join(commonFixture.newPath, ".git"), { recursive: true });
  await mkdir(admin);
  await writeFile(path.join(commonFixture.newPath, ".git"), `gitdir: ${admin}\n`);
  await writeFile(path.join(admin, "commondir"), "x".repeat(1024));
  const commonReport = await migrateSkillsV4({
    configDir: commonFixture.configDir,
    stateDir: commonFixture.stateDir,
    limits: { maxFileBytes: 512 },
  });
  assert.equal(commonReport.status, "failed");
  assert.equal(commonReport.failures[0].code, "resource_limit");
});

test("bounds transformed tracker bytes and field multiplicity before any mutation", async (t) => {
  const bytesFixture = await fixture(t);
  const bytesTicket = path.join(
    bytesFixture.skillsRoot,
    "backlog",
    "backlog",
    "tasks",
    "task-expanded.md",
  );
  const shortOldPath = "/o";
  await writeJson(path.join(bytesFixture.configDir, "migration.json"), {
    oldPath: shortOldPath,
    newPath: bytesFixture.newPath,
  });
  const bytesOriginal = `${Array.from(
    { length: 300 },
    (_, index) => `Claimed worktree: ${shortOldPath}/w${index}`,
  ).join("\n")}\n`;
  await writeFile(bytesTicket, bytesOriginal);
  const bytesReport = await migrateSkillsV4({
    configDir: bytesFixture.configDir,
    stateDir: bytesFixture.stateDir,
    limits: { maxFileBytes: 12 * 1024 },
  });
  assert.equal(bytesReport.status, "failed");
  assert.ok(bytesReport.failures.some(({ code }) => code === "resource_limit"));
  assert.equal(await readFile(bytesTicket, "utf8"), bytesOriginal);

  const fieldsFixture = await fixture(t);
  const validPlacement = path.join(
    fieldsFixture.skillsRoot,
    "placements",
    "TASK-valid.json",
  );
  const validPlacementOriginal = {
    schema: "skills-ticket-placement/v1",
    worktree: fieldsFixture.oldPath,
    gitCommonDirectory: path.join(fieldsFixture.oldPath, ".git"),
    gitDirectory: path.join(fieldsFixture.oldPath, ".git", "worktrees", "valid"),
  };
  await writeJson(validPlacement, validPlacementOriginal);
  const fieldsTicket = path.join(
    fieldsFixture.skillsRoot,
    "backlog",
    "backlog",
    "tasks",
    "task-fields.md",
  );
  const fieldsOriginal = `${Array.from(
    { length: 20 },
    (_, index) => `Claimed worktree: ${path.join(fieldsFixture.oldPath, `w${index}`)}`,
  ).join("\n")}\n`;
  await writeFile(fieldsTicket, fieldsOriginal);
  const fieldsReport = await migrateSkillsV4({
    configDir: fieldsFixture.configDir,
    stateDir: fieldsFixture.stateDir,
    limits: { maxFields: 10 },
  });
  assert.equal(fieldsReport.status, "failed");
  assert.ok(fieldsReport.failures.some(({ code }) => code === "resource_limit"));
  assert.deepEqual(JSON.parse(await readFile(validPlacement, "utf8")), validPlacementOriginal);
  assert.equal(await readFile(fieldsTicket, "utf8"), fieldsOriginal);
});

test("bounds fallback output when many planning failures and report delivery fail", async (t) => {
  const f = await fixture(t);
  const outside = path.join(f.root, "outside.json");
  await writeFile(outside, "{}\n");
  for (let index = 0; index < 50; index += 1) {
    await symlink(
      outside,
      path.join(f.skillsRoot, "placements", `TASK-link-${index}.json`),
    );
  }

  const report = await migrateSkillsV4({
    configDir: f.configDir,
    stateDir: f.stateDir,
    limits: { maxReportBytes: 2 * 1024 },
    testHooks: {
      beforeWriteReport: async () => {
        throw new Error("simulated report failure");
      },
    },
  });
  const summary = compactMigrationSummary(report, 2 * 1024);

  assert.equal(report.summary.changedFiles, 0);
  assert.ok(report.failures.some(({ code }) => code === "resource_limit"));
  assert.ok(Buffer.byteLength(`${JSON.stringify(summary)}\n`) <= 2 * 1024);
});

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
