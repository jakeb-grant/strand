# The docs site on Railway (deploy/railway/README.md): the site CI built
# and checked, served by Caddy. scripts/site/package.sh puts this file,
# the Caddyfile and the site (as site/) in one directory, which CI's
# deploy job uploads with `railway up`; nothing is built here but the
# brotli copies Caddy serves to a reader who accepts them (it can only
# gzip as it goes).
FROM alpine:3.22.2@sha256:4b7ce07002c69e8f3d704a9c5d6fd3053be500b7f1c69fc0d80990c2ad8dd412 AS brotli
RUN apk add --no-cache brotli
COPY site /srv
RUN find /srv -type f -size +1k \( -name '*.html' -o -name '*.md' -o -name '*.txt' \
      -o -name '*.css' -o -name '*.js' -o -name '*.json' -o -name '*.svg' \
      -o -name '*.wasm' -o -name '*.ttf' \) -exec brotli -q 11 -k {} +

FROM caddy:2.11.7-alpine@sha256:eb4d35b7452d1ab6171fbdbe8d9275b2301dad6dad92115d91e19fb9b24d510f
COPY Caddyfile /etc/caddy/Caddyfile
COPY --from=brotli /srv /srv
