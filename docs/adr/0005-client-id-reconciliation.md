# ADR 0005: PWA-generated IDs and reconciliation

The PWA creates a UUID and stores the draft/body hash before submission. Wonder commits `(device_id, client_message_id)` before calling App Server and forwards the UUID as `turn/start.clientUserMessageId`. Ambiguous delivery is reconciled by matching `ThreadItem.userMessage.clientId`; no automatic duplicate turn is permitted.
