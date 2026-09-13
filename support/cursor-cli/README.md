# Cursor CLI through local BYOK

This opt-in launcher supports the official **native Windows** Cursor CLI. It is
source tooling, not an automatic desktop installer feature. Keep both files
(`cursor-byok.ps1` and `launcher.cjs`) together. It does not replace `agent`,
install the CLI, change PATH, or copy provider API keys.

## Setup and use

1. Install the official native Windows CLI following
   [Cursor's installation instructions](https://cursor.com/docs/cli/installation).
   Its standard package location is `%LOCALAPPDATA%\cursor-agent\versions`.
2. Build/run Cursor BYOK from this branch (including the CLI metadata route
   fixes), configure a model, and enable Cursor integration. Keep it running.
3. From the repository root, start a new CLI session:

   ```powershell
   & .\support\cursor-cli\cursor-byok.ps1 --list-models
   & .\support\cursor-cli\cursor-byok.ps1 --model <hash-from-list>
   ```

   Use a configured model's hash from the list. No provider ID or model hash is
   built into the launcher. To work in another directory, invoke the script by
   its absolute path or pass the CLI's `--workspace` option. The CLI may save a
   selection made with `--model` as its new default.

The launcher selects the latest installed version, reads the helper's current
service port from its SQLite database in read-only mode, and checks integration
status before starting. The CLI uses `%USERPROFILE%\.cursor-byok-v3\cli` as an
independent configuration directory. First use creates HTTP/1.1 and allowlist
approval settings; existing settings are preserved. Native Windows does not
implement the CLI sandbox, so this initial configuration disables that sandbox
while retaining tool approval. No `--force` or unrestricted approval is added.

Model requests go to the local service. Other CLI traffic uses its local proxy,
and the public BYOK CA is trusted only by the child process. The built-in local
identity is held in the CLI's memory credential store, not written to the normal
Cursor account credential store. The shell's proxy variables are unchanged.

This relies on Cursor CLI runtime settings verified with
`2026.09.10-fd3934a`; future CLI changes may require updating the launcher.
If the helper is stopped or integration is disabled, startup fails explicitly.
To stop using this setup, run the original CLI directly. The optional BYOK CLI
configuration can be backed up separately; the helper's model database remains
the single source of provider configuration.

## Validation and platform status

Run the launcher checks with Node 24 (or the CLI's bundled Node):

```powershell
node --test support/cursor-cli/launcher.test.cjs
```

Windows end-to-end checks used real Grok and Claude provider configurations,
issued a `Read` tool call, returned the exact contents of a local probe file,
and verified completed local BYOK provider/run records. Model listing alone
is not an end-to-end check.

The desktop project already builds macOS applications for Apple Silicon and
Intel. Its published v0.1.7 packages predate these changes. This branch's changes
need a new macOS build and real CLI validation before claiming equivalent Mac
support. The launcher above intentionally supports Windows only; do not run it
under macOS PowerShell or copy the Windows executable to a Mac.
