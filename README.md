# hyperutils

Small servers that each offer one useful service as a
[hypereffect](https://crates.io/crates/hypereffect) app, for the
[hyperb](../hyperb) browser. Each one is its own crate under `crates/`, and
they all answer at `hyperb://<host>` over HTTPS. Callers are recognized by
their public key, which the transport proves, so there are no logins.

| Crate        | What it is                                                                          | Default listen   |
| ------------ | ----------------------------------------------------------------------------------- | ---------------- |
| `bbs`        | An old-school bulletin board: message boards, mail, teleconference. Also on iroh.   | `127.0.0.1:8401` |
| `meet`       | A Doodle-style scheduler. Share a code, mark the times that suit you, see answers live. | `127.0.0.1:8402` |
| `forms`      | A form builder. Share `/<id>`, collect answers, read them at `/<id>/results`. Forms expire after 1 day to 1 month. | `127.0.0.1:8403` |
| `hyperserve` | The library they share: the HTTP(S) and iroh transports, and `run_app`.             |                  |

## Running

```
cargo run --release -p meet -- --listen 0.0.0.0:443 --tls-cert fullchain.pem --tls-key privkey.pem
```

Renderers use `https` for every host except `localhost` and IP addresses. So a
public server either gets `--tls-cert` and `--tls-key`, or runs on plain HTTP
behind a proxy that terminates TLS. The certificate is read again every 12
hours, so renewals take effect without a restart. Opening the address in a web
browser shows a page that links to it in hyperb.

`--help` on each binary lists its other options.

## Adding a server

Make a new crate under `crates/` that depends on `hyperserve`. Write the app
as a `hypereffect::App`, build it from the caller's key, and serve it:

```rust
hyperserve::http::serve("name", listen, &tls, |session| {
    let app = MyApp::new(session.key);
    hyperserve::run_app(session, app)
})
```

`session.path` is the path of the address the caller opened, and
`session.origin` is `hyperb://<host>[:port]` as they dialed it, so an app can
route on its path and write links to itself (see `crates/forms`).

A server that also needs to react to things outside the session (as the BBS
does with its shared events) can read `session.input` and write
`session.output` directly. See `crates/bbs/src/session.rs`.

`hyperb-core` is a path dependency on `../hyperb`, so that repository has to
be checked out next to this one.

## Releases

Tag to publish:

```
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/publish.yml` then builds every binary crate in the workspace
as a static musl executable (`x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`) and attaches `bbs-v0.1.0-<target>`, its
`.sha256`, and a combined `SHA256SUMS` to the release. Nothing lists the crates
by hand — the matrix comes from `cargo metadata`, so a new crate under `crates/`
ships with the next tag. The library crate (`hyperserve`) has no binary and is
skipped.

CI clones hyperb into `../hyperb` from `<owner>/hyperb` on `main`; override with
the `HYPERB_REPO` / `HYPERB_REF` repository variables, and set a `HYPERB_TOKEN`
secret if that repository is private. A manual `workflow_dispatch` run builds
the same binaries as run artifacts, without a release.
