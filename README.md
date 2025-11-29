memserve is a web server that serves static web content from memory. It loads all content into memory at startup and pre-computes gzip-compressed variants and ETags, so request handling requires no dynamic memory allocation.

Limitations:
- GET requests only, HTTP/1.1 only
- No TLS, no chunked encoding, no pipelining
- ETags only (no Last-Modified), If-None-Match only (no If-Modified-Since)
- Gzip for text-based content types only, Accept headers ignored
- Connections always persistent, closed after 60s idle
- Max path length: 256 bytes
- Max request size: 4KiB
- Max file size: 32MiB
- Max directory depth: 8
- Max connections: 1024
- Single-threaded
- No Date header (allowed per HTTP/1.1 spec 14.18.1 for clockless servers)
