# The load generator. Separate from every server under test so that all of
# them contend with it identically.
FROM alpine:3.21

RUN apk add --no-cache bash curl wrk

WORKDIR /workspace

CMD ["sleep", "infinity"]
