-- Same length as range-64k.lua, with a nonzero file offset.
wrk.method = "GET"
wrk.headers["Range"] = "bytes=1048576-1114111"
wrk.headers["Accept-Encoding"] = "identity"
