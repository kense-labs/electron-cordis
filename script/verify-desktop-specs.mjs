import { spawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access, mkdir, readFile, realpath, rm, stat, writeFile } from 'node:fs/promises';
import { join, relative, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const specRoot = resolve(import.meta.dirname, '../spec');
const invocationDir = process.env.CORDIS_PARITY_CWD || process.cwd();

function compareOutcomes(upstream, candidate) {
  const differences = [];
  for (const id of new Set([...upstream.keys(), ...candidate.keys()])) {
    const expected = upstream.get(id) ?? [];
    const actual = candidate.get(id) ?? [];
    if (expected.length !== actual.length || expected.some((status, index) => status !== actual[index])) {
      differences.push({ id, upstream: expected, candidate: actual });
    }
  }
  return differences;
}

function testOutcomes(report) {
  const outcomes = new Map();
  const add = (id, status) => {
    const statuses = outcomes.get(id) ?? [];
    statuses.push(status);
    outcomes.set(id, statuses);
  };
  for (const suite of report.testResults) {
    const file = relative(specRoot, suite.name);
    const project = suite.projectName ?? null;
    add(JSON.stringify([file, project]), suite.status);
    for (const test of suite.assertionResults) {
      const { line, column } = test.location ?? {};
      add(JSON.stringify([file, project, line, column, test.fullName]), test.status);
    }
  }
  for (const statuses of outcomes.values()) statuses.sort();
  return outcomes;
}

function runnerIssue(report, code, signal) {
  if (signal) return `Spec runner exited on ${signal}`;
  if (!report.runStatus) return 'No Vitest run status recorded';
  const { reason, unhandledErrorCount } = report.runStatus;
  if (reason === 'interrupted' || unhandledErrorCount !== 0) {
    return `Vitest run ended as ${reason} with ${unhandledErrorCount} unhandled errors`;
  }
  if (!(report.numPassedTests || report.numFailedTests) || !report.testResults.length) {
    return 'No upstream specs were executed';
  }
  if (code !== 0 && (code !== 1 || !(report.numFailedTests || report.numFailedTestSuites))) {
    return `Spec runner exited ${code} with no matching test failures`;
  }
  return null;
}

async function runSpecs(binary, reportPath, { filters, testNamePattern, projects, workerIdleTimeoutMs }) {
  const env = {
    ...process.env,
    ELECTRON_SPEC_ELECTRON_PATH: binary,
    ELECTRON_SPEC_JSON_REPORT: reportPath,
    ELECTRON_SPEC_KEEP_RUNNING_INSTANCES: '1',
    ELECTRON_SPEC_WORKERS: '1'
  };
  delete env.ELECTRON_RUN_AS_NODE;
  delete env.NODE_OPTIONS;
  delete env.ELECTRON_SPEC_JUNIT_FILE;
  if (workerIdleTimeoutMs !== undefined) {
    env.ELECTRON_SPEC_WORKER_IDLE_TIMEOUT_MS = String(workerIdleTimeoutMs);
  }
  const args = [
    join(specRoot, 'node_modules/vitest/vitest.mjs'),
    'run',
    '--config', join(specRoot, 'vitest.config.ts'),
    '--reporter=default',
    '--reporter=' + join(specRoot, 'vitest/parity-reporter.mjs'),
    ...filters
  ];
  for (const project of projects) args.push(`--project=${project}`);
  if (testNamePattern !== undefined) args.push(`--testNamePattern=${testNamePattern}`);
  await rm(reportPath, { force: true });
  console.log(`Running specs with ${binary}`);
  let termination;
  try {
    termination = await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, args, { cwd: specRoot, env, stdio: 'inherit' });
      child.once('error', reject);
      child.once('exit', (code, signal) => resolve({ code, signal }));
    });
  } catch (error) {
    return { report: null, issue: `Could not start the spec runner (${error.message})` };
  }
  let report;
  let outcomes;
  try {
    report = JSON.parse(await readFile(reportPath, 'utf8'));
    if (!Array.isArray(report?.testResults)) throw new Error('Expected testResults in the report');
    outcomes = testOutcomes(report);
  } catch (error) {
    const { code, signal } = termination;
    const status = signal ? `on ${signal}` : `with code ${code}`;
    return { report: null, termination, issue: `Runner exited ${status} without a usable report (${error.message})` };
  }
  return { report, outcomes, termination, issue: runnerIssue(report, termination.code, termination.signal) };
}

function summarizeRun(binary, run) {
  const { report, termination, issue } = run;
  return {
    binary,
    total: report?.numTotalTests,
    passed: report?.numPassedTests,
    failed: report?.numFailedTests,
    skipped: report?.numPendingTests,
    todo: report?.numTodoTests,
    termination,
    runStatus: report?.runStatus,
    issue
  };
}

async function main() {
  const [reference, candidate, reports, ...specArgs] = process.argv.slice(2);
  if (!reference || !candidate || !reports) {
    throw new Error('Usage: just verify-desktop-specs <upstream-electron> <candidate-electron> <reports> [--project <name>] [--test-name-pattern <regexp>] [--worker-idle-timeout <ms>] [spec files]');
  }
  const { values, positionals: filters } = parseArgs({
    args: specArgs,
    options: {
      project: { type: 'string', multiple: true },
      'test-name-pattern': { type: 'string' },
      'worker-idle-timeout': { type: 'string' }
    },
    allowPositionals: true
  });
  if (filters.some((filter) => filter.startsWith('-'))) {
    throw new Error('Expected spec filenames after the report directory.');
  }
  const idleLimit = values['worker-idle-timeout'] ?? process.env.ELECTRON_SPEC_WORKER_IDLE_TIMEOUT_MS;
  const workerIdleTimeoutMs = idleLimit === undefined ? undefined : Number(idleLimit);
  if (workerIdleTimeoutMs !== undefined &&
      (!Number.isInteger(workerIdleTimeoutMs) || workerIdleTimeoutMs < 1 || workerIdleTimeoutMs > 2_147_483_647)) {
    throw new Error('Worker idle timeout must be a positive number of milliseconds in the Node timer range');
  }
  const selection = {
    filters,
    projects: values.project ?? [],
    testNamePattern: values['test-name-pattern'],
    workerIdleTimeoutMs
  };
  const upstream = await realpath(resolve(invocationDir, reference));
  const actual = await realpath(resolve(invocationDir, candidate));
  const upstreamInfo = await stat(upstream);
  const candidateInfo = await stat(actual);
  if (!upstreamInfo.isFile() || !candidateInfo.isFile()) {
    throw new Error('Both Electron paths must point to executable files.');
  }
  if (upstream === actual || (upstreamInfo.ino === candidateInfo.ino && upstreamInfo.dev === candidateInfo.dev)) {
    throw new Error('Provide distinct upstream and candidate executables.');
  }
  for (const binary of [upstream, actual]) await access(binary, constants.X_OK);
  const output = resolve(invocationDir, reports);
  await mkdir(output, { recursive: true });
  await rm(join(output, 'summary.json'), { force: true });
  const expected = await runSpecs(upstream, join(output, 'upstream.json'), selection);
  const result = await runSpecs(actual, join(output, 'candidate.json'), selection);
  const comparable = !expected.issue && !result.issue;
  const differences = expected.outcomes && result.outcomes
    ? compareOutcomes(expected.outcomes, result.outcomes)
    : null;
  const summary = {
    upstream: summarizeRun(upstream, expected),
    candidate: summarizeRun(actual, result),
    ...selection,
    comparable,
    differences
  };
  await writeFile(join(output, 'summary.json'), JSON.stringify(summary, null, 2) + '\n');
  if (!comparable) {
    for (const [label, run] of [['upstream', expected], ['candidate', result]]) {
      if (run.issue) console.error(`[${label}] ${run.issue}`);
    }
    console.error(`Spec comparison is inconclusive. See ${output}`);
  } else {
    console.log(`Spec comparison found ${differences.length} outcome differences. See ${output}`);
  }
  process.exitCode = comparable && differences.length === 0 ? 0 : 1;
}

try {
  await main();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
