---
title: Configuration Reference
description: Configuration fields, path resolution, defaults, and precedence.
---

# Configuration Reference

The default configuration path is `$XDG_CONFIG_HOME/trace-index/config.toml`, falling back to `~/.config/trace-index/config.toml`.

If no configuration exists, the default database is `~/.local/share/trace-index/index.sqlite` and the root list is empty.

```toml
database = "~/.local/share/trace-index/index.sqlite"

[[roots]]
name = "codex"
path = "~/.codex/sessions"

[[roots]]
name = "pi"
path = "~/.pi/agent/sessions"

[[roots]]
name = "claude"
path = "~/.claude/projects"
```

Fields:

- `database`: path to the rebuildable SQLite index.
- `roots`: zero or more named files or directories recursively searched for `.jsonl` Sources.
- `roots[].name`: diagnostic label for one root.
- `roots[].path`: Source file or directory path.

Relative paths are resolved against the configuration file directory. A leading `~` expands to the current user's home directory.

Precedence:

1. `--db` overrides the configured database.
2. `--config` selects an explicit configuration file.
3. Otherwise the default configuration path is loaded when present.
4. Built-in defaults fill values not supplied by a file.

`trace-index config show` returns the fully resolved effective configuration.
