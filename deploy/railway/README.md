# The docs site on Railway

The site is built and checked in CI and only served on Railway: CI's
`deploy` job (`.github/workflows/ci.yml`) runs on a push to `main` once
every other job has passed. It takes the `site-live` artifact (the site
with its live examples, which the `wasm` job held to eval's frames),
packages it with this directory's files (`scripts/site/package.sh`),
checks the server, and uploads the package with `railway up`. Railway
builds only the image in `Dockerfile`: Caddy and the files.

- `Dockerfile`: Caddy 2.11.7, pinned by digest, with the site in `/srv`
  and a brotli copy (`.br`) of each text file and the module over 1 kB.
- `Caddyfile`: listens on `$PORT` (Railway terminates TLS); serves the
  brotli copy to a reader who accepts it and gzips the rest as it goes;
  everything is revalidated on every visit except the content-hashed
  files in `wasm/`, kept for a year; the markdown copies are
  `text/markdown; charset=utf-8` and the installer, `/install.sh`
  (`scripts/install.sh`), `text/plain; charset=utf-8`; a missing page
  is a small 404 page.
- `railway.toml`: build with the Dockerfile, health check on `/`.

## Setting it up (once)

1. In the Railway project, add an empty service (no repository connected:
   if a service is connected to the GitHub repo, turn its automatic
   deploys off, or Railway tries to build the Rust workspace).
2. Make a project token for the project's production environment.
3. In the GitHub repository's settings, add the secret `RAILWAY_TOKEN`
   (the token) and the variable `RAILWAY_SERVICE` (the service's name).
4. On the service, add the custom domain. Behind Cloudflare's proxy, add
   a cache rule for the hostname that sets Browser TTL to "Respect
   origin": Cloudflare's own browser TTL (4 hours by default) would
   otherwise replace `no-cache`, and a returning reader could keep an old
   `wasm/manifest.js` naming a module the new deploy no longer has.

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
