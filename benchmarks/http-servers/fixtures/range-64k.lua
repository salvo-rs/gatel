-- The first 64 KiB of a large file: what a video player asks for when
-- somebody drags the scrubber.
wrk.method = "GET"
wrk.headers["Range"] = "bytes=0-65535"
wrk.headers["Accept-Encoding"] = "identity"
