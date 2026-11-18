package main

// Raw TCP HTTP/1.1 server: no net/http, no header maps, no per-request
// allocations beyond the response itself. One goroutine per connection;
// requests on a connection are handled strictly in order (the suite's clients
// are sequential per connection, concurrent across connections).

import (
	"bytes"
	"errors"
	"fmt"
	"net"
	"strconv"
	"strings"
)

type httpServer struct {
	handler func(req *httpRequest) *httpResponse
}

type httpRequest struct {
	method  string
	rawPath string // decoded path (no query)
	rawQ    string // raw query string
	query   map[string][]string
	body    []byte
	host    string
	// The only request headers the forge reads, kept as plain fields so
	// parsing never builds a header map.
	auth         string
	gitProtocol  string
	contentLength int // -1 when the request carries no Content-Length
	chunked      bool
	// keepAlive is what the client asked for; the server may still close.
	keepAlive bool
}

type httpResponse struct {
	status  int
	headers [][2]string // extra headers, in order
	body    []byte
	// contentType overrides the default application/json (git endpoints).
	contentType string
	// headOnly: a HEAD request — send the headers (with the body's
	// content-length) but no body bytes.
	headOnly bool
	// noContentType suppresses the default application/json content-type
	// (used for 204/empty replies that must carry no content-type).
	noContentType bool
	// noContentLength omits content-length (204/304).
	noContentLength bool
}

var reasonPhrases = map[int]string{
	200: "OK",
	201: "Created",
	202: "Accepted",
	204: "No Content",
	400: "Bad Request",
	401: "Unauthorized",
	404: "Not Found",
	405: "Method Not Allowed",
	409: "Conflict",
	422: "Unprocessable Entity",
	500: "Internal Server Error",
}

func (s *httpServer) listenAndServe(addr string) error {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}
	return s.serveListener(ln)
}

func (s *httpServer) serveListener(ln net.Listener) error {
	for {
		conn, err := ln.Accept()
		if err != nil {
			// The listener was closed (shutdown): stop accepting.
			if ne, ok := err.(net.Error); ok && !ne.Temporary() {
				return err
			}
			continue
		}
		go s.serveConn(conn)
	}
}

func (s *httpServer) serveConn(conn net.Conn) {
	defer conn.Close()
	if tc, ok := conn.(*net.TCPConn); ok {
		tc.SetNoDelay(true)
	}
	buf := make([]byte, 0, 8192)
	tmp := make([]byte, 8192)
	for {
		// Parse one request from buf; read more when incomplete.
		for {
			req, consumed, needMore, err := parseRequest(buf)
			if err != nil {
				// Malformed request line/headers: answer 400 if we can, then close.
				writeResponse(conn, &httpResponse{status: 400, body: []byte(`{"message":"Bad Request","documentation_url":"https://example.invalid/docs"}`)}, false)
				return
			}
			if !needMore {
				// Handle and respond, then continue with the remaining bytes.
				resp := s.handler(req)
				keepAlive := req.keepAlive
				if !writeResponse(conn, resp, keepAlive) {
					return
				}
				buf = append(buf[:0], buf[consumed:]...)
				if !keepAlive {
					return
				}
				break
			}
			n, err := conn.Read(tmp)
			if err != nil {
				return
			}
			buf = append(buf, tmp[:n]...)
			if len(buf) > 8<<20 {
				return
			}
		}
	}
}

// writeResponse serializes one response. Returns false when the socket is gone.
func writeResponse(conn net.Conn, resp *httpResponse, keepAlive bool) bool {
	reason, ok := reasonPhrases[resp.status]
	if !ok {
		reason = "Status"
	}
	// The whole head fits in a stack buffer (status line + a few small
	// headers); no bytes.Buffer, no growth.
	var headBuf [512]byte
	head := headBuf[:0]
	head = append(head, "HTTP/1.1 "...)
	head = strconv.AppendInt(head, int64(resp.status), 10)
	head = append(head, ' ')
	head = append(head, reason...)
	head = append(head, "\r\n"...)
	if !resp.noContentType {
		if resp.contentType != "" {
			head = append(head, "content-type: "...)
			head = append(head, resp.contentType...)
			head = append(head, "\r\n"...)
		} else {
			head = append(head, "content-type: application/json\r\n"...)
		}
	}
	for _, h := range resp.headers {
		head = append(head, h[0]...)
		head = append(head, ": "...)
		head = append(head, h[1]...)
		head = append(head, "\r\n"...)
	}
	if !resp.noContentLength {
		head = append(head, "content-length: "...)
		head = strconv.AppendInt(head, int64(len(resp.body)), 10)
		head = append(head, "\r\n"...)
	}
	if keepAlive {
		head = append(head, "connection: keep-alive\r\n"...)
	} else {
		head = append(head, "connection: close\r\n"...)
	}
	head = append(head, "\r\n"...)
	if resp.headOnly || len(resp.body) == 0 {
		_, err := conn.Write(head)
		return err == nil
	}
	// Single writev for header + body.
	bufs := net.Buffers{head, resp.body}
	_, err := bufs.WriteTo(conn)
	return err == nil
}

// parseRequest parses one HTTP request from buf. It returns the request, the
// bytes consumed, whether more data is needed, and a parse error.
//
// Allocation discipline: the request line costs two strings (method, target);
// headers are scanned as bytes and only the four the forge reads are kept
// (authorization, git-protocol, host, content-length); the query map is built
// only when a query string is present.
func parseRequest(buf []byte) (req *httpRequest, consumed int, needMore bool, err error) {
	headEnd := bytes.Index(buf, []byte("\r\n\r\n"))
	if headEnd < 0 {
		if len(buf) > 65536 {
			return nil, 0, false, errors.New("headers too large")
		}
		return nil, 0, true, nil
	}
	head := buf[:headEnd]
	lineEnd := bytes.Index(head, []byte("\r\n"))
	var reqLine []byte
	if lineEnd < 0 {
		reqLine = head
	} else {
		reqLine = head[:lineEnd]
	}
	sp1 := bytes.IndexByte(reqLine, ' ')
	sp2 := bytes.LastIndexByte(reqLine, ' ')
	if sp1 < 0 || sp2 <= sp1 {
		return nil, 0, false, errors.New("bad request line")
	}
	version := reqLine[sp2+1:]
	if !bytes.Equal(version, []byte("HTTP/1.1")) && !bytes.Equal(version, []byte("HTTP/1.0")) {
		return nil, 0, false, errors.New("bad http version")
	}
	method := string(reqLine[:sp1])
	target := string(reqLine[sp1+1 : sp2])
	// Strip absolute-form (scheme://host) if present.
	if strings.HasPrefix(target, "http://") || strings.HasPrefix(target, "https://") {
		rest := target[strings.Index(target, "://")+3:]
		slash := strings.IndexByte(rest, '/')
		if slash < 0 {
			target = "/"
		} else {
			target = rest[slash:]
		}
	}
	rawPath, rawQ := target, ""
	if q := strings.IndexByte(target, '?'); q >= 0 {
		rawPath = target[:q]
		rawQ = target[q+1:]
	}
	decodedPath, err := pathUnescape(rawPath)
	if err != nil {
		return nil, 0, false, fmt.Errorf("bad path escape: %w", err)
	}
	req = &httpRequest{
		method:        method,
		rawPath:       decodedPath,
		rawQ:          rawQ,
		contentLength: -1,
		// keep-alive default: HTTP/1.1 yes, HTTP/1.0 no.
		keepAlive: bytes.Equal(version, []byte("HTTP/1.1")),
	}
	if rawQ != "" {
		req.query = parseQuery(rawQ)
	}
	// Headers: scan line by line; keep only what the forge reads.
	pos := 0
	if lineEnd >= 0 {
		pos = lineEnd + 2
	}
	for pos < len(head) {
		eol := bytes.Index(head[pos:], []byte("\r\n"))
		var line []byte
		if eol < 0 {
			line = head[pos:]
			pos = len(head)
		} else {
			line = head[pos : pos+eol]
			pos += eol + 2
		}
		colon := bytes.IndexByte(line, ':')
		if colon < 0 {
			continue
		}
		name := line[:colon]
		value := bytes.TrimSpace(line[colon+1:])
		switch {
		case bytes.EqualFold(name, []byte("authorization")):
			if req.auth == "" {
				req.auth = string(value)
			}
		case bytes.EqualFold(name, []byte("git-protocol")):
			if req.gitProtocol == "" {
				req.gitProtocol = string(value)
			}
		case bytes.EqualFold(name, []byte("host")):
			if req.host == "" {
				req.host = string(value)
			}
		case bytes.EqualFold(name, []byte("content-length")):
			if req.contentLength < 0 {
				cl, aerr := strconv.Atoi(string(value))
				if aerr != nil || cl < 0 {
					return nil, 0, false, errors.New("bad content-length")
				}
				req.contentLength = cl
			}
		case bytes.EqualFold(name, []byte("transfer-encoding")):
			if bytes.EqualFold(value, []byte("chunked")) {
				req.chunked = true
			}
		case bytes.EqualFold(name, []byte("connection")):
			if bytes.EqualFold(value, []byte("close")) {
				req.keepAlive = false
			} else if bytes.EqualFold(value, []byte("keep-alive")) {
				req.keepAlive = true
			}
		}
	}
	// Expect: 100-continue is not sent by this suite's clients (undici fetch
	// uses Content-Length bodies); it is not handled.
	bodyStart := headEnd + 4
	if req.contentLength >= 0 {
		cl := req.contentLength
		if len(buf) < bodyStart+cl {
			return nil, 0, true, nil
		}
		req.body = buf[bodyStart : bodyStart+cl]
		return req, bodyStart + cl, false, nil
	}
	if req.chunked {
		body, used, ok := parseChunked(buf[bodyStart:])
		if !ok {
			return nil, 0, true, nil
		}
		req.body = body
		return req, bodyStart + used, false, nil
	}
	// No body framing: no body (GET/HEAD and friends).
	req.body = nil
	return req, bodyStart, false, nil
}

// parseChunked decodes a chunked body. Returns the body, bytes consumed, ok.
func parseChunked(buf []byte) ([]byte, int, bool) {
	var out []byte
	pos := 0
	for {
		eol := bytes.Index(buf[pos:], []byte("\r\n"))
		if eol < 0 {
			return nil, 0, false
		}
		line := string(buf[pos : pos+eol])
		if semi := strings.IndexByte(line, ';'); semi >= 0 {
			line = line[:semi]
		}
		size, err := strconv.ParseInt(strings.TrimSpace(line), 16, 64)
		if err != nil || size < 0 {
			return nil, 0, false
		}
		pos += eol + 2
		if size == 0 {
			// trailers until an empty line
			for {
				eol2 := bytes.Index(buf[pos:], []byte("\r\n"))
				if eol2 < 0 {
					return nil, 0, false
				}
				if eol2 == 0 {
					pos += 2
					return out, pos, true
				}
				pos += eol2 + 2
			}
		}
		if len(buf) < pos+int(size)+2 {
			return nil, 0, false
		}
		out = append(out, buf[pos:pos+int(size)]...)
		pos += int(size) + 2
	}
}

// pathUnescape decodes %XX sequences (like decodeURIComponent for a path).
func pathUnescape(s string) (string, error) {
	if !strings.Contains(s, "%") {
		return s, nil
	}
	var out []byte
	for i := 0; i < len(s); i++ {
		if s[i] != '%' {
			out = append(out, s[i])
			continue
		}
		if i+2 >= len(s) {
			return "", errors.New("bad escape")
		}
		hi := hexValByte(s[i+1])
		lo := hexValByte(s[i+2])
		if hi < 0 || lo < 0 {
			return "", errors.New("bad escape")
		}
		out = append(out, byte(hi<<4|lo))
		i += 2
	}
	return string(out), nil
}

func hexValByte(c byte) int {
	switch {
	case c >= '0' && c <= '9':
		return int(c - '0')
	case c >= 'a' && c <= 'f':
		return int(c-'a') + 10
	case c >= 'A' && c <= 'F':
		return int(c-'A') + 10
	}
	return -1
}

// parseQuery parses an application/x-www-form-urlencoded query string the way
// URLSearchParams does: '&' separated, '+' is a space, %XX decoded, a part
// without '=' is a key with an empty value.
func parseQuery(q string) map[string][]string {
	out := make(map[string][]string)
	if q == "" {
		return out
	}
	for _, part := range strings.Split(q, "&") {
		if part == "" {
			continue
		}
		key, value := part, ""
		if eq := strings.IndexByte(part, '='); eq >= 0 {
			key, value = part[:eq], part[eq+1:]
		}
		key = queryUnescape(key)
		value = queryUnescape(value)
		out[key] = append(out[key], value)
	}
	return out
}

func queryUnescape(s string) string {
	if !strings.ContainsAny(s, "+%") {
		return s
	}
	var out []byte
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c == '+' {
			out = append(out, ' ')
		} else if c == '%' && i+2 < len(s) {
			hi := hexValByte(s[i+1])
			lo := hexValByte(s[i+2])
			if hi >= 0 && lo >= 0 {
				out = append(out, byte(hi<<4|lo))
				i += 2
			} else {
				out = append(out, c)
			}
		} else {
			out = append(out, c)
		}
	}
	return string(out)
}

// queryGet returns the first value for a key, or "".
func queryGet(q map[string][]string, key string) string {
	if v, ok := q[key]; ok && len(v) > 0 {
		return v[0]
	}
	return ""
}
