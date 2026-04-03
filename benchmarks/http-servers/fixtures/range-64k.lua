-- Request the first 64KB of the file via Range header
wrk.method = "GET"
wrk.headers["Range"] = "bytes=0-65535"
wrk.headers["Accept-Encoding"] = "identity"
