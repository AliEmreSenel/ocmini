# Security

ocmini is a terminal agent: it prints text it did not write. Assistant text, tool
results, fetched web pages, MCP server responses, hook output, model-written
session titles, file paths, prompt text and slash-command arguments all reach the
transcript, and the transcript is written to a real terminal.

## Transcript escapes

The transcript paints **only** the SGR sequences in `tui::STYLES` (reset, dim,
bold, and the six colours). Every other control sequence is consumed and dropped
before printing, in both render modes and on the full-screen export path
(`Display::output_as` and `wrap_row`, the only two writers of transcript text).

Dropped, so untrusted text cannot:

- restyle the terminal, including the `\x1b[m` reset, which is dropped rather
  than tracked — a style that survives one row keeps replaying on every wrapped
  row, and `commit_row` re-seeds each new row from it;
- hide the cursor (`\x1b[?25l`), erase the display (`\x1b[2J`), move the cursor,
  or enable the mouse / bracketed paste;
- open an OSC 8 hyperlink, set the window title, or write to the clipboard.

Sequences that are not SGR are consumed from their `ESC [` to the terminator, so
an injected OSC, DCS or `ESC`-introduced sequence never reaches a terminal
emulator; what is left of the payload prints as visible text. The draft input is
filtered separately by `clean_input`.

Adding styling means adding a constant to `STYLES`. Do not widen the filter to
whole escape families: the point is that the app, not the text, decides what the
terminal is told to do.

## Threat model

The provider is trusted, its **output is not**: a tool result, a fetched page, an
MCP response, a hook, a model-written title, a filename or a diff can all carry a
prompt-injection payload, and the agent is expected to read the transcript. The
transcript's contract is that printing it cannot alter terminal behaviour. The
provider, config file, `--agent` endpoint and the session store are trusted, and
ocmini runs with the user's own permissions and has no privilege boundary of its
own.

The browser interface served by `ocmini serve` has the same trust level as the HTTP API: anyone who can reach the server, and passes the optional password, can prompt the agent and read or write files as the server's OS user. Its file and settings routes bypass agent permissions and the sandbox, like a local editor: they can change `opencode.json`, which configures providers, MCP commands and hooks. The server binds to loopback by default; set `OPENCODE_SERVER_PASSWORD` before exposing it. The interface's file-changing routes accept only JSON bodies, which a cross-site page cannot send without a CORS preflight that the server never grants. The page uses a restrictive Content-Security-Policy, cannot be framed, and inserts model, tool and file text as text, or as escaped markdown with only http(s) links.

The README describes tool approval and the explicit opt-in for checkout-defined MCP configuration. Tool permissions apply to operations routed through the harness. Approved shell commands and trusted MCP processes run with the account’s OS permissions.
