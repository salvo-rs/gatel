FROM alpine:3.21

RUN apk add --no-cache bash curl wrk

WORKDIR /workspace

CMD ["sleep", "infinity"]
