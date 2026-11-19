# Privacy

Broadcast does not send board content or usage telemetry to the project
maintainers, and it includes no advertising or analytics service.

## Data stored by the server

The Broadcast server stores its database and attachments in its data directory
(`BROADCAST_DATA_DIR`, `.data` by default) on the machine that runs it. Anyone
who can reach the server can read the board it serves.

## Agent runners

`broadcast runner claude` and `broadcast runner codex` run on the user's own
computer. They send the delegated issue's title, description and comments to
the local Claude Code or Codex CLI, which uses Anthropic or OpenAI services under
the user's existing account and that provider's terms. The runner posts the
agent's answer back to the board as a comment.

Broadcast never asks for, receives or stores agent credentials or API keys; the
agent CLI keeps its own sign-in on the user's computer.

## Removing data

Stop the server and delete its data directory to remove the board's data.
