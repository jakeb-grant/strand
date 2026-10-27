# The docs site on Railway

The site is built and checked in CI and only served on Railway: CI's
`deploy` job (`.github/workflows/ci.yml`) runs on a push to `main` once
every other job has passed. It takes the `site-live` artifact (the site
with its live examples, which the `wasm` job held to eval's frames),
packages it with this directory's files (`scripts/site/package.sh`),
checks the server, and uploads the package with `railway up`. Railway
builds only the image in `Dockerfile`: Caddy and the files.

- `Dockerfile`: Caddy 2.11.7, pinned by digest, with the site in `/srv`.
- `Caddyfile`: listens on `$PORT` (Railway terminates TLS); compresses;
  pages, `llms*.txt`, the markdown copies and `wasm/manifest.js` are
  revalidated on every visit, the content-hashed files in `wasm/` kept
  for a year, everything else for an hour; the markdown copies are
  `text/markdown; charset=utf-8`; a missing page is a small 404 page.
- `railway.toml`: build with the Dockerfile, health check on `/`.

## Setting it up (once)

1. In the Railway project, add an empty service (no repository connected:
   if a service is connected to the GitHub repo, turn its automatic
   deploys off, or Railway tries to build the Rust workspace).
2. Make a project token for the project's production environment.
3. In the GitHub repository's settings, add the secret `RAILWAY_TOKEN`
   (the token) and the variable `RAILWAY_SERVICE` (the service's name).
4. On the service, add the custom domain.

Until the secret exists the job prints a notice and deploys nothing.

## Trying it locally

```sh
scripts/site/serve.sh --no-watch     # or any built site with wasm/
scripts/site/package.sh --check target/site target/railway
```

`--check` builds the image and asks the server, in a container with no
network, for a page, `llms.txt`, a markdown copy, the module and a
missing page, and fails unless each has the status, type, compression
and caching above.
