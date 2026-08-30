#!/usr/bin/env node

import { access, readFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';

const output = resolve('doc_build');
const required = [
  'index.html',
  'llms.txt',
  'llms-full.txt',
  'start-here.md',
  'design/domain-model.md',
  'reference/public-schema.md',
];

for (const path of required) {
  await access(join(output, path));
}

const index = await readFile(join(output, 'llms.txt'), 'utf8');
for (const expected of ['Trace Index', 'Start Here', 'Public SQL Schema']) {
  if (!index.includes(expected)) {
    throw new Error(`llms.txt is missing ${JSON.stringify(expected)}`);
  }
}

const full = await readFile(join(output, 'llms-full.txt'), 'utf8');
if (!full.includes('Source') || !full.includes('Semantic')) {
  throw new Error('llms-full.txt is missing the core domain model');
}

const html = await readFile(join(output, 'start-here.html'), 'utf8');
if (!html.includes('llms') || !html.includes('Start Here')) {
  throw new Error('Start Here HTML is missing the Rspress Agent UI surface');
}

process.stdout.write('RSPress Agent Friendly output verified\n');
