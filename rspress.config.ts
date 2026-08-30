import { defineConfig } from '@rspress/core';

export default defineConfig({
  root: 'docs',
  base: '/trace-index/',
  title: 'Trace Index',
  description:
    'A bounded, source-traceable query plane for Codex, Pi, and Claude Code traces.',
  llms: true,
  themeConfig: {
    llmsUI: {
      injectLlmsHint: true,
      viewOptions: ['markdownLink', 'chatgpt', 'claude'],
    },
  },
});
