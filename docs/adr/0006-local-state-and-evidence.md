# ADR 0006: Local ownership, structured logs, and recorded-first evidence

Wonder state and logs remain local under the documented macOS Application Support and Logs directories. Logs are JSONL, bounded, and redacted. Recorded protocol tests use a fake App Server and consume no model usage; live tests remain opt-in and are never implied by recorded evidence.
