-- Cross the 64 KiB inline/streamed boundary without changing the source file.
wrk.method = "GET"
wrk.headers["Range"] = "bytes=0-66559"
wrk.headers["Accept-Encoding"] = "identity"
