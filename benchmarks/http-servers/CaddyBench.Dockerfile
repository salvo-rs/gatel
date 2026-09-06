# Caddy's own binary on a plain base, so the comparison is between servers
# rather than between the base images their publishers happened to choose.
FROM caddy:2.9-alpine AS source

FROM debian:bookworm-slim

COPY --from=source /usr/bin/caddy /usr/bin/caddy

EXPOSE 8080

ENTRYPOINT ["caddy"]
CMD ["run", "--config", "/etc/caddy/Caddyfile"]
