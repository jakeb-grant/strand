# The docs site on Railway (deploy/railway/README.md): the site CI built
# and checked, served by Caddy. scripts/site/package.sh puts this file,
# the Caddyfile and the site (as site/) in one directory, which CI's
# deploy job uploads with `railway up`; nothing is built here.
FROM caddy:2.11.7-alpine@sha256:eb4d35b7452d1ab6171fbdbe8d9275b2301dad6dad92115d91e19fb9b24d510f
COPY Caddyfile /etc/caddy/Caddyfile
COPY site /srv
