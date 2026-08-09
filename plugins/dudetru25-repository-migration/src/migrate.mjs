import { randomUUID } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import {
  lstat,
  mkdir,
  open,
  readdir,
  rename,
  unlink,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const CONFIG_FILE = "migration.json";
const REPORT_FILE = "migration-report.json";
const CONSUMER_SKILLS = "skills-v4";
const CONSUMER_CLAUDE = "claude-projects";
const CONSUMER_EDITORS = "editor-workspaces";
const CONSUMER_REPORT = "report";
const FULL_CONFIG_KEYS = [
  "claudeProjectsRoot",
  "claudeRootQuiescent",
  "editorWorkspaceFiles",
  "newPath",
  "newProjectKey",
  "oldPath",
  "oldProjectKey",
];
const PLACEMENT_SCHEMA = "skills-ticket-placement/v1";
const SNAPSHOT_SCHEMAS = new Set(["skills-snapshot/v4", "skills-snapshot/v5"]);
const DEFAULT_LIMITS = Object.freeze({
  maxDepth: 32,
  maxEntries: 4096,
  maxFiles: 512,
  maxFileBytes: 1024 * 1024,
  maxTotalBytes: 32 * 1024 * 1024,
  maxFields: 4096,
  maxReportBytes: 48 * 1024,
});
const MAX_FAILURE_DETAIL_LENGTH = 512;

export async function migrateSkillsV4({ configDir, stateDir, limits, testHooks = {} }) {
  const report = createReport();
  const effectiveLimits = { ...DEFAULT_LIMITS, ...limits };

  try {
    requireDirectoryOption(configDir, "configDir");
    requireDirectoryOption(stateDir, "stateDir");
    await mkdir(stateDir, { recursive: true });
    await requireRegularDirectory(configDir, "invalid_config");
    await requireRegularDirectory(stateDir, "invalid_state_path");

    const config = await loadConfig(path.join(configDir, CONFIG_FILE), effectiveLimits);
    report.oldPath = config.oldPath;
    report.newPath = config.newPath;

    const skillsRoot = await resolveSkillsRoot(config.newPath, effectiveLimits);
    await preflightMigrationInputs(skillsRoot, effectiveLimits);
    const traversal = {
      depth: 0,
      entries: 0,
      files: 0,
      totalBytes: 0,
      plannedBytes: 0,
      fields: 0,
    };
    const plan = [];
    await planJsonDirectory({
      directory: path.join(skillsRoot, "placements"),
      kind: "placement",
      report,
      skillsRoot,
      traversal,
      limits: effectiveLimits,
      testHooks,
      plan,
    });
    await planJsonDirectory({
      directory: path.join(skillsRoot, "snapshots"),
      kind: "snapshot",
      report,
      skillsRoot,
      traversal,
      limits: effectiveLimits,
      testHooks,
      plan,
    });
    await planTrackerDirectory({
      directory: path.join(skillsRoot, "backlog"),
      report,
      skillsRoot,
      traversal,
      limits: effectiveLimits,
      testHooks,
      plan,
    });

    const planningExceededLimits = report.failures.some(
      ({ code }) => code === "resource_limit",
    );
    if (plan.length === 0 && !planningExceededLimits) {
      report.failures.push({
        path: relativeReportPath(skillsRoot, skillsRoot),
        code: "no_matching_paths",
        detail: `no supported Skills V4 value is within ${config.oldPath}`,
      });
    }
    if (!planningExceededLimits && reservePlannedReportSpace(report, plan, effectiveLimits)) {
      for (const item of plan) {
        try {
          await replaceFileAtomically(
            item.file,
            item.original,
            item.replacement,
            item.originalStat,
            testHooks,
            effectiveLimits.maxFileBytes,
          );
          recordChange(report, item.reportPath, item.fields);
        } catch (error) {
          report.failures.push(
            failureRecord(
              item.reportPath,
              error?.migrationCode ?? "migration_failed",
              error instanceof Error ? error.message : String(error),
            ),
          );
        }
      }
    }
  } catch (error) {
    report.failures.push(normalizeFailure(error));
  }

  finalizeReport(report);
  boundUnappliedReport(report, effectiveLimits);
  report.delivery = { status: "unavailable", reportPath: null };
  if (typeof stateDir === "string" && stateDir.length > 0) {
    try {
      report.delivery = { status: "written", reportPath: REPORT_FILE };
      ensureReportSize(report, effectiveLimits);
      await testHooks.beforeWriteReport?.(report);
      await writeReport(path.join(stateDir, REPORT_FILE), report);
    } catch (error) {
      report.delivery = { status: "failed", reportPath: null };
      report.failures.push(normalizeFailure(error, REPORT_FILE, "report_write_failed"));
      finalizeReport(report);
    }
  }
  return report;
}

export async function migrateRepositoryPaths({ configDir, stateDir, limits, testHooks = {} }) {
  const report = createRepositoryReport();
  const effectiveLimits = { ...DEFAULT_LIMITS, ...limits };

  try {
    requireDirectoryOption(configDir, "configDir");
    requireDirectoryOption(stateDir, "stateDir");
    await mkdir(stateDir, { recursive: true });
    await requireRegularDirectory(configDir, "invalid_config");
    await requireRegularDirectory(stateDir, "invalid_state_path");

    const config = await loadRepositoryConfig(
      path.join(configDir, CONFIG_FILE),
      effectiveLimits,
    );
    report.oldPath = config.oldPath;
    report.newPath = config.newPath;
    await requireRegularDirectory(config.newPath, "invalid_new_path");

    const traversal = {
      depth: 0,
      entries: 0,
      files: 0,
      totalBytes: 0,
      plannedBytes: 0,
      fields: 0,
    };
    const plan = [];

    await planSkillsConsumer({
      config,
      report,
      traversal,
      limits: effectiveLimits,
      testHooks,
      plan,
    });
    await planClaudeConsumer({
      config,
      report,
      traversal,
      limits: effectiveLimits,
      plan,
    });
    await planEditorConsumers({
      config,
      report,
      traversal,
      limits: effectiveLimits,
      plan,
    });

    const planningExceededLimits = report.failures.some(
      ({ code }) => code === "resource_limit",
    );
    if (
      !planningExceededLimits &&
      reserveRepositoryReportSpace(report, plan, effectiveLimits)
    ) {
      for (const item of plan) {
        try {
          if (item.kind === "claude-directory") {
            await renameClaudeProjectAtomically(item, testHooks);
          } else {
            await replaceFileAtomically(
              item.file,
              item.original,
              item.replacement,
              item.originalStat,
              testHooks,
              effectiveLimits.maxFileBytes,
            );
          }
          recordRepositoryChange(report, item.consumer, item.reportPath, item.fields);
        } catch (error) {
          report.failures.push(
            repositoryFailureRecord(
              item.consumer,
              item.reportPath,
              error?.migrationCode ?? "migration_failed",
              error instanceof Error ? error.message : String(error),
            ),
          );
        }
      }
    }
  } catch (error) {
    report.failures.push(
      repositoryFailureRecord(
        "migration",
        error?.migrationPath ?? ".",
        error?.migrationCode ?? "migration_failed",
        error instanceof Error ? error.message : String(error),
      ),
    );
  }

  finalizeRepositoryReport(report);
  boundUnappliedRepositoryReport(report, effectiveLimits);
  report.delivery = { status: "unavailable", reportPath: null };
  if (typeof stateDir === "string" && stateDir.length > 0) {
    try {
      report.delivery = { status: "written", reportPath: REPORT_FILE };
      ensureReportSize(report, effectiveLimits);
      await testHooks.beforeWriteReport?.(report);
      await writeReport(path.join(stateDir, REPORT_FILE), report);
    } catch (error) {
      report.delivery = { status: "failed", reportPath: null };
      report.failures.push(
        repositoryFailureRecord(
          CONSUMER_REPORT,
          REPORT_FILE,
          error?.migrationCode ?? "report_write_failed",
          error instanceof Error ? error.message : String(error),
        ),
      );
      finalizeRepositoryReport(report);
    }
  }
  return report;
}

function createRepositoryReport() {
  return {
    schema: "repository-migration-report/v1",
    status: "failed",
    oldPath: null,
    newPath: null,
    changed: [],
    failures: [],
    consumers: {
      [CONSUMER_SKILLS]: emptyConsumerSummary(),
      [CONSUMER_CLAUDE]: emptyConsumerSummary(),
      [CONSUMER_EDITORS]: emptyConsumerSummary(),
    },
    summary: {
      changedFiles: 0,
      changedValues: 0,
      failures: 0,
    },
    delivery: { status: "pending", reportPath: null },
  };
}

function emptyConsumerSummary() {
  return {
    status: "unchanged",
    changedFiles: 0,
    changedValues: 0,
    failures: 0,
  };
}

function finalizeRepositoryReport(report) {
  report.changed.sort((left, right) =>
    `${left.consumer}:${left.path}`.localeCompare(`${right.consumer}:${right.path}`),
  );
  report.failures.sort((left, right) =>
    `${left.consumer}:${left.path}`.localeCompare(`${right.consumer}:${right.path}`),
  );
  report.summary = {
    changedFiles: report.changed.length,
    changedValues: report.changed.reduce(
      (total, change) => total + change.fields.length,
      0,
    ),
    failures: report.failures.length,
  };
  for (const consumer of [CONSUMER_SKILLS, CONSUMER_CLAUDE, CONSUMER_EDITORS]) {
    const changes = report.changed.filter((change) => change.consumer === consumer);
    const failures = report.failures.filter((failureEntry) =>
      failureEntry.consumer === consumer,
    );
    report.consumers[consumer] = {
      status:
        failures.length === 0
          ? changes.length === 0
            ? "unchanged"
            : "success"
          : changes.length === 0
            ? "failed"
            : "partial",
      changedFiles: changes.length,
      changedValues: changes.reduce(
        (total, change) => total + change.fields.length,
        0,
      ),
      failures: failures.length,
    };
  }
  report.status =
    report.failures.length === 0
      ? "success"
      : report.changed.length === 0
        ? "failed"
        : "partial";
}

async function loadRepositoryConfig(configPath, limits) {
  const text = await readRegularFile(configPath, "invalid_config", limits.maxFileBytes);
  let value;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw failure("invalid_config", CONFIG_FILE, `invalid JSON: ${error.message}`);
  }
  const keys = isPlainObject(value) ? Object.keys(value).sort() : [];
  if (
    keys.length !== FULL_CONFIG_KEYS.length ||
    keys.some((key, index) => key !== FULL_CONFIG_KEYS[index])
  ) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      `full migration config must contain only ${FULL_CONFIG_KEYS.join(", ")}`,
    );
  }

  validateMigrationPair(value.oldPath, value.newPath);
  validateMigrationPath(value.claudeProjectsRoot, "claudeProjectsRoot");
  if (value.claudeRootQuiescent !== true) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      "claudeRootQuiescent must be true after all Claude processes using the projects root are stopped",
    );
  }
  validateClaudeProjectKey(value.oldProjectKey, "oldProjectKey");
  validateClaudeProjectKey(value.newProjectKey, "newProjectKey");
  if (
    !Array.isArray(value.editorWorkspaceFiles) ||
    value.editorWorkspaceFiles.length === 0
  ) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      "editorWorkspaceFiles must be a non-empty array",
    );
  }
  if (value.editorWorkspaceFiles.length > limits.maxFiles) {
    throw failure(
      "resource_limit",
      CONFIG_FILE,
      "editor workspace file count exceeds the file limit",
    );
  }
  const seen = new Set();
  for (const [index, file] of value.editorWorkspaceFiles.entries()) {
    validateMigrationPath(file, `editorWorkspaceFiles[${index}]`);
    if (path.extname(file) !== ".code-workspace") {
      throw failure(
        "invalid_config",
        CONFIG_FILE,
        `editorWorkspaceFiles[${index}] must name a .code-workspace file`,
      );
    }
    if (seen.has(file)) {
      throw failure(
        "invalid_config",
        CONFIG_FILE,
        `editorWorkspaceFiles contains duplicate path ${file}`,
      );
    }
    seen.add(file);
  }
  return value;
}

function validateMigrationPair(oldPath, newPath) {
  validateMigrationPath(oldPath, "oldPath");
  validateMigrationPath(newPath, "newPath");
  if (oldPath === newPath) {
    throw failure("invalid_config", CONFIG_FILE, "oldPath and newPath must differ");
  }
  if (path.parse(oldPath).root === oldPath) {
    throw failure("invalid_config", CONFIG_FILE, "oldPath must not be a filesystem root");
  }
  if (isPathWithin(oldPath, newPath) || isPathWithin(newPath, oldPath)) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      "oldPath and newPath must not contain one another",
    );
  }
}

function validateClaudeProjectKey(value, field) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value === "." ||
    value === ".." ||
    value.includes("/") ||
    value.includes("\\") ||
    value.includes("\0") ||
    Buffer.byteLength(value) > 255 ||
    path.basename(value) !== value
  ) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      `${field} must be a safe basename of at most 255 bytes`,
    );
  }
}

async function planSkillsConsumer({
  config,
  report,
  traversal,
  limits,
  testHooks,
  plan,
}) {
  const failures = [];
  const consumerReport = {
    oldPath: config.oldPath,
    newPath: config.newPath,
    failures,
    newMatches: 0,
  };
  const planStart = plan.length;
  try {
    const skillsRoot = await resolveSkillsRoot(config.newPath, limits);
    await planJsonDirectory({
      directory: path.join(skillsRoot, "placements"),
      kind: "placement",
      report: consumerReport,
      skillsRoot,
      traversal,
      limits,
      testHooks,
      plan,
    });
    await planJsonDirectory({
      directory: path.join(skillsRoot, "snapshots"),
      kind: "snapshot",
      report: consumerReport,
      skillsRoot,
      traversal,
      limits,
      testHooks,
      plan,
    });
    await planTrackerDirectory({
      directory: path.join(skillsRoot, "backlog"),
      report: consumerReport,
      skillsRoot,
      traversal,
      limits,
      testHooks,
      plan,
    });
  } catch (error) {
    failures.push(normalizeFailure(error));
  }
  if (
    plan.length === planStart &&
    consumerReport.newMatches === 0 &&
    failures.length === 0
  ) {
    failures.push(
      failureRecord(
        ".",
        "no_matching_paths",
        "no supported Skills V4 value uses the old or new repository path",
      ),
    );
  }
  for (let index = planStart; index < plan.length; index += 1) {
    plan[index].consumer = CONSUMER_SKILLS;
    plan[index].kind = "file";
  }
  report.failures.push(
    ...failures.map((failureEntry) => ({
      consumer: CONSUMER_SKILLS,
      ...failureEntry,
    })),
  );
}

async function planClaudeConsumer({ config, report, traversal, limits, plan }) {
  try {
    await requireRegularDirectory(config.claudeProjectsRoot, "unsafe_file_type");
    consumeEntry(traversal, limits);
    consumeEntry(traversal, limits);
    const source = path.join(config.claudeProjectsRoot, config.oldProjectKey);
    const destination = path.join(config.claudeProjectsRoot, config.newProjectKey);
    const sourceStat = await optionalLstat(source);
    const destinationStat = source === destination
      ? sourceStat
      : await optionalLstat(destination);

    if (source === destination) {
      if (!sourceStat) {
        throw failure(
          "source_missing",
          source,
          "Claude project source and destination are both missing",
        );
      }
      requireSafeDirectoryStat(sourceStat, source);
      return;
    }
    if (!sourceStat && destinationStat) {
      requireSafeDirectoryStat(destinationStat, destination);
      throw failure(
        "unverifiable_destination",
        destination,
        "Claude project destination exists but this run cannot prove it owns the migration",
      );
    }
    if (!sourceStat) {
      throw failure("source_missing", source, "Claude project source is missing");
    }
    requireSafeDirectoryStat(sourceStat, source);
    if (destinationStat) {
      if (destinationStat.isSymbolicLink()) {
        throw failure(
          "unsafe_file_type",
          destination,
          "Claude project destination must not be a symbolic link",
        );
      }
      throw failure(
        "destination_exists",
        destination,
        "Claude project destination already exists",
      );
    }
    plan.push({
      kind: "claude-directory",
      consumer: CONSUMER_CLAUDE,
      source,
      destination,
      sourceStat,
      reportPath: destination,
      fields: ["projectDirectory"],
    });
  } catch (error) {
    report.failures.push(
      repositoryFailureRecord(
        CONSUMER_CLAUDE,
        error?.migrationPath ?? config.claudeProjectsRoot,
        error?.migrationCode ?? "migration_failed",
        error instanceof Error ? error.message : String(error),
      ),
    );
  }
}

async function planEditorConsumers({ config, report, traversal, limits, plan }) {
  for (const file of config.editorWorkspaceFiles) {
    consumeEntry(traversal, limits);
    try {
      const result = await planRegularFile(
        file,
        (text) => transformEditorWorkspace(
          text,
          file,
          config.oldPath,
          config.newPath,
          limits,
        ),
        { limits, traversal },
      );
      if (result.fields.length > 0) {
        plan.push({
          ...result,
          kind: "file",
          consumer: CONSUMER_EDITORS,
          reportPath: file,
        });
      } else if ((result.currentFields?.length ?? 0) === 0) {
        report.failures.push(
          repositoryFailureRecord(
            CONSUMER_EDITORS,
            file,
            "no_matching_paths",
            "workspace has no folder path within the old or new repository path",
          ),
        );
      }
    } catch (error) {
      report.failures.push(
        repositoryFailureRecord(
          CONSUMER_EDITORS,
          error?.migrationPath ?? file,
          error?.migrationCode ?? "migration_failed",
          error instanceof Error ? error.message : String(error),
        ),
      );
    }
  }
}

function transformEditorWorkspace(text, file, oldPath, newPath, limits) {
  let record;
  try {
    record = JSON.parse(text);
  } catch (error) {
    throw failure(
      "malformed_workspace",
      file,
      `invalid workspace JSON: ${error.message}`,
    );
  }
  if (!isPlainObject(record) || !Array.isArray(record.folders)) {
    throw failure(
      "malformed_workspace",
      file,
      "workspace must be a JSON object with a folders array",
    );
  }
  if (record.folders.length > limits.maxFields) {
    throw failure(
      "resource_limit",
      file,
      "workspace folder count exceeds the field limit",
    );
  }

  const fields = [];
  const currentFields = [];
  for (const [index, folder] of record.folders.entries()) {
    if (!isPlainObject(folder)) {
      throw failure(
        "malformed_workspace",
        file,
        `folders[${index}] must be an object`,
      );
    }
    const hasPath = Object.hasOwn(folder, "path");
    const hasUri = Object.hasOwn(folder, "uri");
    if (hasPath === hasUri) {
      throw failure(
        "malformed_workspace",
        file,
        `folders[${index}] must contain exactly one of path or uri`,
      );
    }
    if (hasUri) {
      if (typeof folder.uri !== "string" || !isValidWorkspaceUri(folder.uri)) {
        throw failure(
          "malformed_workspace",
          file,
          `folders[${index}].uri must be a valid URI`,
        );
      }
      continue;
    }
    if (
      typeof folder.path !== "string" ||
      folder.path.length === 0 ||
      folder.path.includes("\0")
    ) {
      throw failure(
        "malformed_workspace",
        file,
        `folders[${index}].path must be a non-empty path without NUL bytes`,
      );
    }
    const originalPath = folder.path;
    const absolute = path.isAbsolute(folder.path)
      ? folder.path
      : path.resolve(path.dirname(file), folder.path);
    const replacement = replacePathBoundary(absolute, oldPath, newPath);
    if (replacement === null) {
      if (replacePathBoundary(absolute, newPath, newPath) !== null) {
        currentFields.push(`folders[${index}].path`);
      }
      continue;
    }
    folder.path = path.isAbsolute(folder.path)
      ? replacement
      : path.relative(path.dirname(file), replacement) || ".";
    if (originalPath.includes("/") && !originalPath.includes("\\")) {
      folder.path = folder.path.split(path.sep).join("/");
    }
    fields.push(`folders[${index}].path`);
  }
  return {
    content: fields.length === 0 ? text : `${JSON.stringify(record, null, 2)}\n`,
    fields,
    currentFields,
  };
}

function isValidWorkspaceUri(value) {
  try {
    const parsed = new URL(value);
    return parsed.protocol.length > 1;
  } catch {
    return false;
  }
}

async function renameClaudeProjectAtomically(item, testHooks) {
  const lockPath = path.join(
    path.dirname(item.source),
    ".herdr-repository-migration.lock",
  );
  let lockHandle;
  let lockStat;
  let lockOwned = false;
  let renamed = false;
  try {
    try {
      lockHandle = await open(lockPath, "wx", 0o600);
    } catch (error) {
      if (error?.code === "EEXIST") {
        throw failure(
          "migration_locked",
          lockPath,
          "another Claude project migration holds the cooperative lock",
        );
      }
      throw error;
    }
    lockOwned = true;
    lockStat = await lockHandle.stat();
    await testHooks.beforeClaudeRename?.(item.source, item.destination);
    const currentSource = await safeLstat(item.source, "concurrent_change");
    if (!sameDirectoryIdentity(item.sourceStat, currentSource)) {
      throw failure(
        "concurrent_change",
        item.source,
        "Claude project source changed during migration",
      );
    }
    const destinationStat = await optionalLstat(item.destination);
    if (destinationStat) {
      throw failure(
        "concurrent_change",
        item.destination,
        "Claude project destination appeared during migration",
      );
    }
    await rename(item.source, item.destination);
    renamed = true;
    await testHooks.afterClaudeRenameBeforeVerify?.(
      item.source,
      item.destination,
    );
    const sourceAfter = await optionalLstat(item.source);
    const migrated = await safeLstat(item.destination, "migration_failed");
    if (sourceAfter || !sameDirectoryIdentity(item.sourceStat, migrated)) {
      throw failure(
        "concurrent_change",
        item.destination,
        "Claude project destination appeared or changed during migration",
      );
    }
    await testHooks.afterClaudeRename?.(item.source, item.destination);
  } catch (error) {
    if (renamed) {
      try {
        const sourceNow = await optionalLstat(item.source);
        const destinationNow = await optionalLstat(item.destination);
        if (
          sourceNow ||
          !destinationNow ||
          !sameDirectoryIdentity(item.sourceStat, destinationNow)
        ) {
          throw new Error("filesystem state no longer permits rollback");
        }
        await rename(item.destination, item.source);
      } catch (rollbackError) {
        throw failure(
          "rollback_failed",
          item.destination,
          `Claude project rename failed and rollback failed: ${rollbackError.message}`,
        );
      }
    }
    throw error;
  } finally {
    await lockHandle?.close().catch(() => {});
    if (lockOwned && lockStat) {
      const currentLock = await optionalLstat(lockPath).catch(() => null);
      if (currentLock && sameFileIdentity(lockStat, currentLock)) {
        await unlink(lockPath).catch(() => {});
      }
    }
  }
}

function requireSafeDirectoryStat(stat, directory) {
  if (!stat.isDirectory() || stat.isSymbolicLink()) {
    throw failure(
      "unsafe_file_type",
      directory,
      "Claude project path must be a regular directory",
    );
  }
}

function sameDirectoryIdentity(left, right) {
  return (
    right.isDirectory() &&
    !right.isSymbolicLink() &&
    left.dev === right.dev &&
    left.ino === right.ino
  );
}

async function optionalLstat(file) {
  try {
    return await lstat(file);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

function recordRepositoryChange(report, consumer, file, fields) {
  if (fields.length > 0) {
    report.changed.push({ consumer, path: file, fields });
  }
}

function repositoryFailureRecord(consumer, file, code, detail) {
  return { consumer, ...failureRecord(file, code, detail) };
}

function reserveRepositoryReportSpace(report, plan, limits) {
  const worstCase = {
    ...report,
    changed: plan.map(({ consumer, reportPath, fields }) => ({
      consumer,
      path: reportPath,
      fields,
    })),
    failures: [
      ...report.failures,
      ...plan.map(({ consumer, reportPath }) =>
        repositoryFailureRecord(
          consumer,
          reportPath,
          "concurrent_change",
          "x".repeat(MAX_FAILURE_DETAIL_LENGTH),
        ),
      ),
      repositoryFailureRecord(
        CONSUMER_REPORT,
        REPORT_FILE,
        "report_write_failed",
        "x".repeat(MAX_FAILURE_DETAIL_LENGTH),
      ),
    ],
  };
  finalizeRepositoryReport(worstCase);
  const reportBytes = serializedReport(worstCase).length;
  const fallbackBytes = Buffer.byteLength(
    `${JSON.stringify(compactMigrationSummary(worstCase))}\n`,
  );
  if (reportBytes > limits.maxReportBytes || fallbackBytes > limits.maxReportBytes) {
    report.changed = [];
    report.failures = [
      repositoryFailureRecord(
        "migration",
        REPORT_FILE,
        "resource_limit",
        "migration report or fallback exceeds the byte limit",
      ),
    ];
    return false;
  }
  return true;
}

function boundUnappliedRepositoryReport(report, limits) {
  if (report.changed.length > 0 || serializedReport(report).length <= limits.maxReportBytes) {
    return;
  }
  report.failures = [
    repositoryFailureRecord(
      "migration",
      REPORT_FILE,
      "resource_limit",
      "migration stopped before mutation because its report exceeded the byte limit",
    ),
  ];
  finalizeRepositoryReport(report);
}

function createReport() {
  return {
    schema: "repository-migration-report/v1",
    status: "failed",
    oldPath: null,
    newPath: null,
    changed: [],
    failures: [],
    summary: {
      changedFiles: 0,
      changedValues: 0,
      failures: 0,
    },
    delivery: { status: "pending", reportPath: null },
  };
}

function finalizeReport(report) {
  report.changed.sort((left, right) => left.path.localeCompare(right.path));
  report.failures.sort((left, right) => left.path.localeCompare(right.path));
  report.summary = {
    changedFiles: report.changed.length,
    changedValues: report.changed.reduce((total, change) => total + change.fields.length, 0),
    failures: report.failures.length,
  };
  report.status =
    report.failures.length === 0
      ? "success"
      : report.changed.length === 0
        ? "failed"
        : "partial";
}

async function loadConfig(configPath, limits) {
  const text = await readRegularFile(configPath, "invalid_config", limits.maxFileBytes);
  let value;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw failure("invalid_config", CONFIG_FILE, `invalid JSON: ${error.message}`);
  }

  const keys = isPlainObject(value) ? Object.keys(value).sort() : [];
  if (keys.length !== 2 || keys[0] !== "newPath" || keys[1] !== "oldPath") {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      "migration.json must contain only oldPath and newPath",
    );
  }
  validateMigrationPath(value.oldPath, "oldPath");
  validateMigrationPath(value.newPath, "newPath");
  if (value.oldPath === value.newPath) {
    throw failure("invalid_config", CONFIG_FILE, "oldPath and newPath must differ");
  }
  if (path.parse(value.oldPath).root === value.oldPath) {
    throw failure("invalid_config", CONFIG_FILE, "oldPath must not be a filesystem root");
  }
  if (isPathWithin(value.oldPath, value.newPath) || isPathWithin(value.newPath, value.oldPath)) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      "oldPath and newPath must not contain one another",
    );
  }
  return value;
}

function validateMigrationPath(value, field) {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    !path.isAbsolute(value) ||
    path.normalize(value) !== value
  ) {
    throw failure(
      "invalid_config",
      CONFIG_FILE,
      `${field} must be a normalized absolute path`,
    );
  }
}

async function resolveSkillsRoot(newPath, limits) {
  await requireRegularDirectory(newPath, "invalid_new_path");
  const dotGit = path.join(newPath, ".git");
  const dotGitStat = await safeLstat(dotGit, "invalid_new_path");
  let commonDirectory;

  if (dotGitStat.isDirectory()) {
    commonDirectory = dotGit;
  } else if (dotGitStat.isFile()) {
    const pointer = (await readRegularFile(dotGit, "invalid_new_path", limits.maxFileBytes)).trim();
    const match = /^gitdir: (.+)$/.exec(pointer);
    if (!match) {
      throw failure("invalid_new_path", dotGit, "malformed .git pointer");
    }
    const gitDirectory = path.resolve(newPath, match[1]);
    await requireRegularDirectory(gitDirectory, "invalid_new_path");
    const commonPointer = path.join(gitDirectory, "commondir");
    try {
      const commonStat = await lstat(commonPointer);
      if (!commonStat.isFile() || commonStat.isSymbolicLink()) {
        throw failure("invalid_new_path", commonPointer, "unsafe commondir pointer");
      }
      const commonValue = (
        await readRegularFile(commonPointer, "invalid_new_path", limits.maxFileBytes)
      ).trim();
      if (commonValue.length === 0) {
        throw failure("invalid_new_path", commonPointer, "empty commondir pointer");
      }
      commonDirectory = path.resolve(gitDirectory, commonValue);
    } catch (error) {
      if (error?.code === "ENOENT") {
        commonDirectory = gitDirectory;
      } else {
        throw error;
      }
    }
  } else {
    throw failure("invalid_new_path", dotGit, ".git is not a regular file or directory");
  }

  await requireRegularDirectory(commonDirectory, "invalid_new_path");
  const skillsRoot = path.join(commonDirectory, "skills-v4");
  await requireRegularDirectory(skillsRoot, "missing_skills_state");
  return skillsRoot;
}

async function planJsonDirectory({
  directory,
  kind,
  report,
  skillsRoot,
  traversal,
  limits,
  testHooks,
  plan,
}) {
  const entries = await optionalDirectoryEntries(directory, report, skillsRoot);
  for (const entry of entries) {
    consumeEntry(traversal, limits);
    const file = path.join(directory, entry.name);
    const reportPath = relativeReportPath(skillsRoot, file);
    if (entry.isSymbolicLink()) {
      report.failures.push(
        failureRecord(reportPath, "unsafe_file_type", "symbolic links are not followed"),
      );
      continue;
    }
    if (!entry.isFile() || path.extname(entry.name) !== ".json") {
      continue;
    }

    try {
      const result = await planRegularFile(
        file,
        (text) => transformJsonState(text, kind, report.oldPath, report.newPath, reportPath),
        { limits, traversal },
      );
      report.newMatches = (report.newMatches ?? 0) + (result.currentFields?.length ?? 0);
      if (result.fields.length > 0) {
        plan.push({ ...result, reportPath });
      }
    } catch (error) {
      report.failures.push(normalizeFailure(error, reportPath));
    }
  }
}

function transformJsonState(text, kind, oldPath, newPath, reportPath) {
  let record;
  try {
    record = JSON.parse(text);
  } catch (error) {
    throw failure("malformed_state", reportPath, `invalid JSON: ${error.message}`);
  }
  if (!isPlainObject(record)) {
    throw failure("malformed_state", reportPath, "state record must be a JSON object");
  }

  const fields = kind === "placement" ? placementFields(record, reportPath) : snapshotFields(record, reportPath);
  const changed = [];
  const currentFields = [];
  for (const field of fields) {
    const replacement = replacePathBoundary(record[field], oldPath, newPath);
    if (replacement !== null) {
      record[field] = replacement;
      changed.push(field);
    } else if (replacePathBoundary(record[field], newPath, newPath) !== null) {
      currentFields.push(field);
    }
  }
  return {
    content: changed.length === 0 ? text : `${JSON.stringify(record, null, 2)}\n`,
    fields: changed,
    currentFields,
  };
}

function placementFields(record, reportPath) {
  if (record.schema !== PLACEMENT_SCHEMA) {
    throw failure(
      "unsupported_schema",
      reportPath,
      `unsupported placement schema: ${String(record.schema)}`,
    );
  }
  const fields = ["worktree", "gitCommonDirectory", "gitDirectory"];
  requireStringFields(record, fields, reportPath);
  return fields;
}

function snapshotFields(record, reportPath) {
  if (!SNAPSHOT_SCHEMAS.has(record.schema)) {
    throw failure(
      "unsupported_schema",
      reportPath,
      `unsupported snapshot schema: ${String(record.schema)}`,
    );
  }
  requireStringFields(record, ["worktree"], reportPath);
  return ["worktree"];
}

function requireStringFields(record, fields, reportPath) {
  for (const field of fields) {
    if (typeof record[field] !== "string") {
      throw failure("malformed_state", reportPath, `${field} must be a string`);
    }
  }
}

async function planTrackerDirectory({
  directory,
  report,
  skillsRoot,
  traversal,
  limits,
  testHooks,
  plan,
  depth = 0,
}) {
  if (depth > limits.maxDepth) {
    throw failure("resource_limit", relativeReportPath(skillsRoot, directory), "tracker depth limit exceeded");
  }
  let entries;
  try {
    const directoryStat = await lstat(directory);
    if (!directoryStat.isDirectory() || directoryStat.isSymbolicLink()) {
      report.failures.push(
        failureRecord(
          relativeReportPath(skillsRoot, directory),
          "unsafe_file_type",
          "tracker directory must not be a symbolic link",
        ),
      );
      return;
    }
    entries = await readdir(directory, { withFileTypes: true });
  } catch (error) {
    if (error?.code === "ENOENT") {
      return;
    }
    throw error;
  }
  entries.sort((left, right) => left.name.localeCompare(right.name));

  for (const entry of entries) {
    consumeEntry(traversal, limits);
    const file = path.join(directory, entry.name);
    const reportPath = relativeReportPath(skillsRoot, file);
    if (entry.isSymbolicLink()) {
      report.failures.push(
        failureRecord(reportPath, "unsafe_file_type", "symbolic links are not followed"),
      );
    } else if (entry.isDirectory()) {
      await planTrackerDirectory({
        directory: file,
        report,
        skillsRoot,
        traversal,
        limits,
        testHooks,
        plan,
        depth: depth + 1,
      });
    } else if (entry.isFile() && path.extname(entry.name) === ".md") {
      try {
        const result = await planRegularFile(
          file,
          (text) => transformTracker(
            text,
            report.oldPath,
            report.newPath,
            reportPath,
            limits,
            traversal,
          ),
          { limits, traversal },
        );
        report.newMatches = (report.newMatches ?? 0) + (result.currentFields?.length ?? 0);
        if (result.fields.length > 0) {
          plan.push({ ...result, reportPath });
        }
      } catch (error) {
        report.failures.push(normalizeFailure(error, reportPath));
      }
    }
  }
}

function transformTracker(text, oldPath, newPath, reportPath, limits, traversal) {
  const fields = [];
  const currentFields = [];
  let projectedBytes = Buffer.byteLength(text);
  const content = text.replace(/^Claimed worktree: ([^\r\n]+)$/gm, (line, value) => {
    const replacement = replacePathBoundary(value, oldPath, newPath);
    if (replacement === null) {
      if (replacePathBoundary(value, newPath, newPath) !== null) {
        currentFields.push("Claimed worktree");
      }
      return line;
    }
    const replacementLine = `Claimed worktree: ${replacement}`;
    projectedBytes += Buffer.byteLength(replacementLine) - Buffer.byteLength(line);
    const projectedFields = fields.length + 1;
    if (
      projectedBytes > limits.maxFileBytes ||
      traversal.plannedBytes + projectedBytes > limits.maxTotalBytes ||
      projectedFields > limits.maxFields ||
      traversal.fields + projectedFields > limits.maxFields
    ) {
      throw failure(
        "resource_limit",
        reportPath,
        "transformed migration data exceeds its limit",
      );
    }
    fields.push("Claimed worktree");
    return replacementLine;
  });
  return { content, fields, currentFields };
}

export function replacePathBoundary(value, oldPath, newPath) {
  const pathApi =
    path.win32.isAbsolute(value) &&
    path.win32.isAbsolute(oldPath) &&
    path.win32.isAbsolute(newPath)
      ? path.win32
      : path;
  if (!pathApi.isAbsolute(value)) {
    return null;
  }
  const relative = pathApi.relative(
    pathApi.normalize(oldPath),
    pathApi.normalize(value),
  );
  if (relative.length === 0) {
    return newPath;
  }
  if (
    relative === ".." ||
    relative.startsWith(`..${pathApi.sep}`) ||
    pathApi.isAbsolute(relative)
  ) {
    return null;
  }
  const replacement = pathApi.join(pathApi.normalize(newPath), relative);
  return pathApi === path.win32 && value.includes("/") && !value.includes("\\")
    ? replacement.replaceAll("\\", "/")
    : replacement;
}

async function planRegularFile(file, transform, { limits, traversal }) {
  const { buffer: original, stat: originalStat } = await readRegularFileBuffer(
    file,
    "unsafe_file_type",
    limits.maxFileBytes,
  );
  traversal.files += 1;
  traversal.totalBytes += original.length;
  if (traversal.files > limits.maxFiles || traversal.totalBytes > limits.maxTotalBytes) {
    throw failure("resource_limit", file, "migration input limit exceeded");
  }
  const result = transform(original.toString("utf8"));
  const replacement =
    result.fields.length === 0 ? original : Buffer.from(result.content);
  traversal.plannedBytes += replacement.length;
  traversal.fields += result.fields.length;
  if (
    replacement.length > limits.maxFileBytes ||
    traversal.plannedBytes > limits.maxTotalBytes ||
    result.fields.length > limits.maxFields ||
    traversal.fields > limits.maxFields
  ) {
    throw failure("resource_limit", file, "transformed migration data exceeds its limit");
  }
  if (result.fields.length === 0) {
    return { ...result, file, original, originalStat, replacement };
  }
  return {
    ...result,
    file,
    original,
    originalStat,
    replacement,
  };
}

async function replaceFileAtomically(
  file,
  original,
  replacement,
  originalStat,
  testHooks = {},
  maxBytes = DEFAULT_LIMITS.maxFileBytes,
) {
  const temporary = path.join(path.dirname(file), `.${path.basename(file)}.${process.pid}.${randomUUID()}.tmp`);
  let temporaryExists = false;
  try {
    const handle = await open(temporary, "wx", originalStat.mode & 0o7777);
    temporaryExists = true;
    try {
      await handle.writeFile(replacement);
      await handle.chmod(originalStat.mode & 0o7777);
      await handle.sync();
    } finally {
      await handle.close();
    }

    await testHooks.beforeReplace?.(file);
    const { buffer: current, stat: currentStat } = await readRegularFileBuffer(
      file,
      "concurrent_change",
      maxBytes,
    );
    if (
      !currentStat.isFile() ||
      currentStat.isSymbolicLink() ||
      currentStat.dev !== originalStat.dev ||
      currentStat.ino !== originalStat.ino ||
      (currentStat.mode & 0o7777) !== (originalStat.mode & 0o7777) ||
      !current.equals(original)
    ) {
      throw failure("concurrent_change", file, "state file changed during migration");
    }
    await rename(temporary, file);
    temporaryExists = false;
  } finally {
    if (temporaryExists) {
      await unlink(temporary).catch(() => {});
    }
  }
}

async function writeReport(file, report) {
  await mkdir(path.dirname(file), { recursive: true });
  let original = null;
  let originalStat = null;
  try {
    const pathStat = await lstat(file);
    if (!pathStat.isFile() || pathStat.isSymbolicLink()) {
      throw failure("report_write_failed", REPORT_FILE, "report path is not a regular file");
    }
    const existing = await readRegularFileBuffer(
      file,
      "report_write_failed",
      DEFAULT_LIMITS.maxReportBytes,
    );
    originalStat = existing.stat;
    original = existing.buffer;
  } catch (error) {
    if (error?.code !== "ENOENT") {
      throw error;
    }
  }

  const content = serializedReport(report);
  if (originalStat) {
    await replaceFileAtomically(file, original, content, originalStat);
    return;
  }

  const temporary = path.join(path.dirname(file), `.${REPORT_FILE}.${process.pid}.${randomUUID()}.tmp`);
  try {
    const handle = await open(temporary, "wx", 0o600);
    try {
      await handle.writeFile(content);
      await handle.sync();
    } finally {
      await handle.close();
    }
    await rename(temporary, file);
  } catch (error) {
    await unlink(temporary).catch(() => {});
    throw error;
  }
}

async function preflightMigrationInputs(skillsRoot, limits) {
  const budget = { depth: 0, entries: 0, files: 0, totalBytes: 0 };
  await preflightDirectory(path.join(skillsRoot, "placements"), skillsRoot, limits, budget, 0, false);
  await preflightDirectory(path.join(skillsRoot, "snapshots"), skillsRoot, limits, budget, 0, false);
  await preflightDirectory(path.join(skillsRoot, "backlog"), skillsRoot, limits, budget, 0, true);
}

async function preflightDirectory(directory, skillsRoot, limits, budget, depth, recursive) {
  if (depth > limits.maxDepth) {
    throw failure("resource_limit", relativeReportPath(skillsRoot, directory), "tracker depth limit exceeded");
  }
  let entries;
  try {
    const directoryStat = await lstat(directory);
    if (!directoryStat.isDirectory() || directoryStat.isSymbolicLink()) {
      return;
    }
    entries = await readdir(directory, { withFileTypes: true });
  } catch (error) {
    if (error?.code === "ENOENT") {
      return;
    }
    throw error;
  }
  for (const entry of entries) {
    budget.entries += 1;
    if (budget.entries > limits.maxEntries) {
      throw failure("resource_limit", relativeReportPath(skillsRoot, directory), "directory entry limit exceeded");
    }
    const candidate = path.join(directory, entry.name);
    if (recursive && entry.isDirectory() && !entry.isSymbolicLink()) {
      await preflightDirectory(candidate, skillsRoot, limits, budget, depth + 1, true);
      continue;
    }
    const accepted = entry.isFile() && (path.extname(entry.name) === ".json" || (recursive && path.extname(entry.name) === ".md"));
    if (!accepted) {
      continue;
    }
    const stat = await safeLstat(candidate, "unsafe_file_type");
    budget.files += 1;
    budget.totalBytes += stat.size;
    if (
      stat.size > limits.maxFileBytes ||
      budget.files > limits.maxFiles ||
      budget.totalBytes > limits.maxTotalBytes
    ) {
      throw failure("resource_limit", relativeReportPath(skillsRoot, candidate), "migration input or report limit exceeded");
    }
  }
}

function consumeEntry(traversal, limits) {
  traversal.entries += 1;
  if (traversal.entries > limits.maxEntries) {
    throw failure("resource_limit", ".", "directory entry limit exceeded");
  }
}

function ensureReportSize(report, limits) {
  if (serializedReport(report).length > limits.maxReportBytes) {
    throw failure("resource_limit", REPORT_FILE, "migration report exceeds the byte limit");
  }
}

function boundUnappliedReport(report, limits) {
  if (report.changed.length > 0 || serializedReport(report).length <= limits.maxReportBytes) {
    return;
  }
  report.failures = [
    failureRecord(
      REPORT_FILE,
      "resource_limit",
      "migration stopped before mutation because its report exceeded the byte limit",
    ),
  ];
  finalizeReport(report);
}

function reservePlannedReportSpace(report, plan, limits) {
  const worstCase = {
    ...report,
    changed: plan.map(({ reportPath, fields }) => ({ path: reportPath, fields })),
    failures: [
      ...report.failures,
      ...plan.map(({ reportPath }) =>
        failureRecord(
          reportPath,
          "concurrent_change",
          "x".repeat(MAX_FAILURE_DETAIL_LENGTH),
        ),
      ),
      failureRecord(
        REPORT_FILE,
        "report_write_failed",
        "x".repeat(MAX_FAILURE_DETAIL_LENGTH),
      ),
    ],
    delivery: { status: "failed", reportPath: null },
  };
  finalizeReport(worstCase);
  const reportBytes = serializedReport(worstCase).length;
  const fallbackBytes = Buffer.byteLength(
    `${JSON.stringify(compactMigrationSummary(worstCase))}\n`,
  );
  if (reportBytes > limits.maxReportBytes || fallbackBytes > limits.maxReportBytes) {
    report.changed = [];
    report.failures = [
      failureRecord(
        REPORT_FILE,
        "resource_limit",
        "migration report or fallback exceeds the byte limit",
      ),
    ];
    return false;
  }
  return true;
}

function serializedReport(report) {
  return Buffer.from(`${JSON.stringify(report, null, 2)}\n`);
}

async function optionalDirectoryEntries(directory, report, skillsRoot) {
  try {
    const directoryStat = await lstat(directory);
    if (!directoryStat.isDirectory() || directoryStat.isSymbolicLink()) {
      report.failures.push(
        failureRecord(
          relativeReportPath(skillsRoot, directory),
          "unsafe_file_type",
          "state directory must not be a symbolic link",
        ),
      );
      return [];
    }
    const entries = await readdir(directory, { withFileTypes: true });
    return entries.sort((left, right) => left.name.localeCompare(right.name));
  } catch (error) {
    if (error?.code === "ENOENT") {
      return [];
    }
    report.failures.push(normalizeFailure(error, relativeReportPath(skillsRoot, directory)));
    return [];
  }
}

async function readRegularFile(file, code, maxBytes = DEFAULT_LIMITS.maxFileBytes) {
  const { buffer } = await readRegularFileBuffer(file, code, maxBytes);
  return buffer.toString("utf8");
}

async function readRegularFileBuffer(file, code, maxBytes) {
  const before = await safeLstat(file, code);
  if (!before.isFile() || before.isSymbolicLink()) {
    throw failure(code, file, "path must be a regular file");
  }
  if (before.size > maxBytes) {
    throw failure("resource_limit", file, "file exceeds the byte limit");
  }

  const flags = fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0);
  let handle;
  try {
    handle = await open(file, flags);
    const opened = await handle.stat();
    if (!sameFileIdentity(before, opened)) {
      throw failure("concurrent_change", file, "file changed before bounded read");
    }

    const chunks = [];
    let total = 0;
    while (total <= maxBytes) {
      const chunk = Buffer.allocUnsafe(Math.min(64 * 1024, maxBytes + 1 - total));
      const { bytesRead } = await handle.read(chunk, 0, chunk.length, null);
      if (bytesRead === 0) {
        break;
      }
      chunks.push(chunk.subarray(0, bytesRead));
      total += bytesRead;
    }
    if (total > maxBytes) {
      throw failure("resource_limit", file, "file exceeds the byte limit");
    }

    const after = await handle.stat();
    const pathAfter = await safeLstat(file, code);
    if (
      !sameFileIdentity(before, after) ||
      !sameFileIdentity(after, pathAfter) ||
      before.size !== after.size ||
      before.mtimeMs !== after.mtimeMs ||
      before.ctimeMs !== after.ctimeMs ||
      (before.mode & 0o7777) !== (after.mode & 0o7777)
    ) {
      throw failure("concurrent_change", file, "file changed during bounded read");
    }
    return { buffer: Buffer.concat(chunks, total), stat: after };
  } catch (error) {
    if (error?.migrationCode) {
      throw error;
    }
    throw failure(code, file, error instanceof Error ? error.message : String(error));
  } finally {
    await handle?.close().catch(() => {});
  }
}

function sameFileIdentity(left, right) {
  return (
    right.isFile() &&
    !right.isSymbolicLink() &&
    left.dev === right.dev &&
    left.ino === right.ino
  );
}

async function requireRegularDirectory(directory, code) {
  const stat = await safeLstat(directory, code);
  if (!stat.isDirectory() || stat.isSymbolicLink()) {
    throw failure(code, directory, "path must be a regular directory");
  }
}

async function safeLstat(file, code) {
  try {
    return await lstat(file);
  } catch (error) {
    throw failure(code, file, error.message);
  }
}

function requireDirectoryOption(value, name) {
  if (typeof value !== "string" || value.length === 0) {
    throw failure("missing_environment", name, `${name} is required`);
  }
}

function recordChange(report, file, fields) {
  if (fields.length > 0) {
    report.changed.push({ path: file, fields });
  }
}

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function relativeReportPath(root, file) {
  const relative = path.relative(root, file);
  return relative.length === 0 ? "." : relative.split(path.sep).join("/");
}

function failure(code, file, detail) {
  const error = new Error(boundFailureDetail(detail));
  error.migrationCode = code;
  error.migrationPath = file;
  return error;
}

function failureRecord(file, code, detail) {
  return { path: file, code, detail: boundFailureDetail(detail) };
}

function boundFailureDetail(detail) {
  const value = String(detail);
  return value.length <= MAX_FAILURE_DETAIL_LENGTH
    ? value
    : `${value.slice(0, MAX_FAILURE_DETAIL_LENGTH - 3)}...`;
}

function isPathWithin(parent, candidate) {
  const relative = path.relative(parent, candidate);
  return relative.length === 0 || (
    relative !== ".." &&
    !relative.startsWith(`..${path.sep}`) &&
    !path.isAbsolute(relative)
  );
}

function normalizeFailure(error, fallbackPath = ".", fallbackCode = "migration_failed") {
  return failureRecord(
    error?.migrationPath ?? fallbackPath,
    error?.migrationCode ?? fallbackCode,
    error instanceof Error ? error.message : String(error),
  );
}

async function main() {
  const mode = migrationModeForArguments(process.argv.slice(2));
  const migrate = mode === "all-consumers"
    ? migrateRepositoryPaths
    : migrateSkillsV4;
  const report = await migrate({
    configDir: process.env.HERDR_PLUGIN_CONFIG_DIR,
    stateDir: process.env.HERDR_PLUGIN_STATE_DIR,
  });
  process.stdout.write(`${JSON.stringify(compactMigrationSummary(report))}\n`);
  process.exitCode = report.status === "success" ? 0 : 1;
}

export function migrationModeForArguments(args) {
  if (args.length === 0 || (args.length === 1 && args[0] === "--skills-v4-only")) {
    return "skills-v4-only";
  }
  if (args.length === 1 && args[0] === "--all-consumers") {
    return "all-consumers";
  }
  throw failure(
    "invalid_arguments",
    "command line",
    "expected --skills-v4-only or --all-consumers",
  );
}

export function compactMigrationSummary(report, maxBytes = DEFAULT_LIMITS.maxReportBytes) {
  if (report.delivery?.status === "written") {
    return { status: report.status, ...report.summary, reportPath: report.delivery.reportPath };
  }
  const summary = {
    status: report.status,
    ...report.summary,
    reportPath: null,
    reportDelivery: report.delivery?.status ?? "unavailable",
    changed: [],
    failures: [],
    truncated: false,
  };
  for (const failureEntry of report.failures) {
    summary.failures.push(failureEntry);
    if (Buffer.byteLength(`${JSON.stringify(summary)}\n`) > maxBytes) {
      summary.failures.pop();
      summary.truncated = true;
      break;
    }
  }
  for (const change of report.changed) {
    summary.changed.push(change);
    if (Buffer.byteLength(`${JSON.stringify(summary)}\n`) > maxBytes) {
      summary.changed.pop();
      summary.truncated = true;
      break;
    }
  }
  return summary;
}

const invokedPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  await main();
}
