#!/usr/bin/env node

import { access, readFile, readdir } from 'node:fs/promises';
import { join, resolve } from 'node:path';

async function listFiles(directory, prefix = '') {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const relative = join(prefix, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listFiles(join(directory, entry.name), relative)));
    } else {
      files.push(relative);
    }
  }

  return files;
}

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

const sourcePages = (await listFiles(resolve('docs')))
  .filter(path => path.endsWith('.md') || path.endsWith('.mdx'))
  .map(path => path.replace(/\.mdx$/, '.md'));

for (const path of sourcePages) {
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
for (const expected of [
  'rp-llms-hint',
  'rp-llms-copy-button',
  'Copy Markdown',
  'rp-llms-view-options__trigger',
  '/trace-index/llms.txt',
  '/trace-index/llms-full.txt',
  '/trace-index/start-here.md',
]) {
  if (!html.includes(expected)) {
    throw new Error(
      `Start Here HTML is missing the Rspress Agent UI marker ${JSON.stringify(expected)}`,
    );
  }
}

const clientScripts = (await listFiles(join(output, 'static', 'js'))).filter(path =>
  path.endsWith('.js'),
);
const clientBundle = (
  await Promise.all(
    clientScripts.map(path => readFile(join(output, 'static', 'js', path), 'utf8')),
  )
).join('\n');

for (const expected of ['ChatGPT', 'chatgpt.com', 'Claude', 'claude.ai']) {
  if (!clientBundle.includes(expected)) {
    throw new Error(
      `RSPress Agent view options are missing ${JSON.stringify(expected)}`,
    );
  }
}

process.stdout.write(
  `RSPress Agent Friendly output verified (${sourcePages.length} Markdown pages)\n`,
);
