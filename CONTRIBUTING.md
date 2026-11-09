# Contributing to Trove

Thanks for helping! Issues and pull requests are welcome, in English or
Chinese. By participating you agree to follow the
[code of conduct](CODE_OF_CONDUCT.md).

Larger changes (a new package format, storage backend or API change) are
easiest to land when discussed in an issue or discussion first.

Contributions are accepted under the project's Apache-2.0 license
(inbound = outbound): by submitting a pull request you confirm that you have
the right to license your contribution under it.

## Development setup

```sh
make web     # build the UI once (or run `npm run dev` in web/ for hot reload)
make run     # server on http://127.0.0.1:8081, admin / admin12345
```

`npm run dev` in `web/` serves the UI on http://localhost:5173 and proxies API
and repository traffic to the server on port 8081.

## Before opening a pull request

```sh
make lint    # rustfmt, clippy -D warnings, TypeScript
make test    # unit + end-to-end tests on SQLite
TROVE_TEST_POSTGRES_URL=postgres://trove:trove@127.0.0.1/trove_test make test-pg
```

* Keep SQL in `trove-db` portable between SQLite and PostgreSQL: `$N`
  placeholders, `TEXT`/`BIGINT` columns, `CAST(... AS BIGINT)` for aggregates.
* New package formats live in `crates/trove-server/src/formats/` and should
  support hosted, proxy and group where the ecosystem allows, plus an
  end-to-end test in `crates/trove-server/tests/e2e.rs`.
* User-facing strings in the UI go through `web/src/i18n.tsx` (English and
  Simplified Chinese).
* Write commit messages in the imperative mood ("Add Go proxy support").

## Reporting security issues

Please do not open public issues for vulnerabilities; see [SECURITY.md](SECURITY.md).
