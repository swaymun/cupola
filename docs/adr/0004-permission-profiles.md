# ADR 0004: Generated named permission profiles

Each Bot receives an ephemeral named App Server permission profile with minimal platform reads, Bot-home writes, explicitly approved extra roots, sensitive-state denial, and network enabled. Bot start/resume uses `approvalPolicy: "never"`: routine workspace shell/network work proceeds without permission clicks, while sandbox denials remain denied. Write roots cannot overlap protected system trees or the whole user home.

Wonder sends `permissions` and `runtimeWorkspaceRoots`; it never combines them with legacy sandbox fields or mutates global Codex configuration. Native computer use controls the host desktop and retains its separate approval boundary until an isolated desktop/VM is implemented and verified.
