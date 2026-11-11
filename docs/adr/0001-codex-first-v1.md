# ADR 0001: Codex-first and ChatGPT-managed v1

Wonder v1 uses the installed Codex App Server as its only model backend and reuses Codex-managed ChatGPT authentication. Wonder never creates a parallel account, reads credential files, or exposes App Server directly. A versioned compatibility manifest and recorded probes gate execution.
