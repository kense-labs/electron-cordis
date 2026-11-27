import { readFile, writeFile } from 'node:fs/promises';
import { relative } from 'node:path';
import { JsonReporter } from 'vitest/node';

const progressInterval = 30_000;

export default class ParityReporter extends JsonReporter {
  #reportPath;
  #progressTimer;
  #activeModules = new Map();
  #activeCases = new Map();

  constructor() {
    const reportPath = process.env.ELECTRON_SPEC_JSON_REPORT;
    if (!reportPath) throw new Error('Set ELECTRON_SPEC_JSON_REPORT for the parity reporter.');
    super({ outputFile: reportPath });
    this.#reportPath = reportPath;
  }

  onTestRunStart() {
    this.#progressTimer = setInterval(() => {
      const active = this.#activeCases.size ? this.#activeCases : this.#activeModules;
      for (const { module, test, startedAt } of active.values()) {
        const file = relative(this.ctx.config.root, module.moduleId);
        const seconds = Math.floor((performance.now() - startedAt) / 1000);
        const label = test ? `${file} > ${test}` : file;
        this.ctx.logger.log(`[cordis-parity] ${module.project.name} ${label} running for ${seconds}s`);
      }
    }, progressInterval).unref();
  }

  onTestModuleStart(module) {
    // Vitest may advance after an unexpected worker exit without an end event.
    if (process.env.ELECTRON_SPEC_WORKERS === '1') {
      this.#activeModules.clear();
      this.#activeCases.clear();
    }
    this.#activeModules.set(module.id, { module, startedAt: performance.now() });
  }

  onTestModuleEnd(module) {
    this.#activeModules.delete(module.id);
    for (const [id, { module: owner }] of this.#activeCases) {
      if (owner.id === module.id) this.#activeCases.delete(id);
    }
  }

  onTestCaseReady(test) {
    this.#activeCases.set(test.id, { module: test.module, test: test.fullName, startedAt: performance.now() });
  }

  onTestCaseResult(test) {
    this.#activeCases.delete(test.id);
  }

  async onTestRunEnd(testModules, unhandledErrors, reason) {
    clearInterval(this.#progressTimer);
    this.#activeCases.clear();
    this.#activeModules.clear();
    await super.onTestRunEnd(testModules);
    // Vitest's JSON reporter omits project names and run-level errors.
    const report = JSON.parse(await readFile(this.#reportPath, 'utf8'));
    if (report.testResults.length !== testModules.length ||
        report.testResults.some((file, index) => file.name !== testModules[index].moduleId)) {
      throw new Error('Vitest JSON report does not match the reported test modules');
    }
    for (const [index, module] of testModules.entries()) {
      report.testResults[index].projectName = module.project.name;
    }
    report.runStatus = { reason, unhandledErrorCount: unhandledErrors.length };
    await writeFile(this.#reportPath, JSON.stringify(report) + '\n');
  }
}
