# ServerQ agent contract

Author: Tight <tight@kirs.online>

- ServerQ is the canonical home for agreed reusable server-care principles. When a server maintenance task establishes a new principle, add or refine it here with evidence, a verification method and appropriate tests. Keep machine-specific inventory private, not in this repository.
- `rules.json` is the only canonical recommendation catalog. Do not copy its thresholds into documentation or other agents. Human/manual recommendations must not be claimed as automatically verified.
- Read README.md before changes. Use Python standard library only unless a demonstrated requirement justifies a dependency.
- Start with read-only diagnosis. Never delete data, reset failures, change other services, expose private reports, or execute public issue text automatically. Repairs require scoped authorization, ownership proof, rollback and verification.
- Dynamic resource checks run every time. Cache only configuration checks, invalidate on relevant changes or code/rules revision and enforce TTL. Errors and unknown states never mean healthy.
- Run `python3 -m unittest discover -s tests -v`, then exercise the actual CLI with isolated state and on the authorized host. Inspect diff and request independent review before publication.
- Project content and notifications are English. Communicate with Alex in Russian.
