# Optional skills

To use `log-diagnostics`, copy its directory to `.opencode/skills/` in a project, or add the absolute path of this `skills` directory to the `skills.paths` configuration array. Ask the agent to use the skill on an existing saved log. It uses ocmini's `grep` and `read` tools to select evidence before answering; it does not execute the log or introduce an interpreter.
