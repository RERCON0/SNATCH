# Dependency review, 2026-10-25

- No open GitHub Dependabot security alerts at review time.
- RUSTSEC-2026-0386: update Tokio 1.53.2 to 1.53.3, also covering the Linux build graph. This integrates the change proposed by Dependabot PR #3.
- Replace yanked clipboard-win 5.4.1 with 5.4.3.
- The ZIP extraction advisories were already addressed by the previous zip 9.0.4 update (commit 9dba20f); retain fixed-name, bounded extraction in the installers.
- Dependabot PR #2 proposes an eframe 0.37 migration; it is a routine 0.x API update, not an open security alert. Keep that compatibility change separate from this patch release.
- PR #4 proposes routine grouped dependency updates; inspect its actual changes independently of the alert list.

Verdict: the remaining version proposals are maintenance updates; the security and yanked-version findings above are checked against the current locked dependency graph.
