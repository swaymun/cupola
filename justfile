default:

fmt:
	cargo fmt --all -- --check

test:
	cargo test --workspace

verify-provenance:
	node scripts/verify-phase0.mjs
