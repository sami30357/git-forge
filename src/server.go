package main

// Request context, routing and shared helpers — the Go equivalent of the Node
// forge's handleRequest / Ctx / sendJson / authorized / paginate.

import (
	"math"
	"sort"
	"strconv"
	"strings"
	"sync"
)

type ctx struct {
	store  *store
	token  string
	method string
	path   string
	query  map[string][]string
	body   []byte
	origin string
	isHead bool
	resp   *httpResponse
	// The only request headers the forge reads.
	auth        string
	gitProtocol string
}

type forgeServer struct {
	store *store
	token string
	mu    sync.Mutex
}

func newForgeServer(token string) *forgeServer {
	return &forgeServer{store: newStore(), token: token}
}

// handle is the per-request entry point: it locks the store for the whole
// routing + response build (the socket write happens outside the lock), then
// returns the response. HEAD responses carry the GET headers but no body.
func (f *forgeServer) handle(req *httpRequest) *httpResponse {
	c := &ctx{
		store:       f.store,
		token:       f.token,
		method:      req.method,
		path:        req.rawPath,
		query:       req.query,
		body:        req.body,
		origin:      "http://" + req.host,
		isHead:      req.method == "HEAD",
		auth:        req.auth,
		gitProtocol: req.gitProtocol,
	}
	if c.origin == "http://" {
		c.origin = "http://127.0.0.1"
	}
	f.mu.Lock()
	resp := f.route(c)
	f.mu.Unlock()
	if c.isHead {
		resp.headOnly = true
	}
	return resp
}

func (f *forgeServer) route(c *ctx) (resp *httpResponse) {
	defer func() {
		if r := recover(); r != nil {
			// The Node forge's catch-all answers 500 with just {"message": ...}.
			w := &jw{}
			w.raw(`{"message":`)
			w.str(errText(r))
			w.raw(`}`)
			resp = c.jsonResp(500, w.bytes(), nil)
		}
	}()
	if c.method == "GET" && c.path == "/healthz" {
		c.resp = c.jsonResp(200, []byte(`{"ok":true}`), nil)
		return c.resp
	}
	if c.path == "/api/graphql" && c.method == "POST" {
		handleGraphQL(c)
		return c.resp
	}
	if c.path == "/mcp" {
		handleMcp(c)
		return c.resp
	}
	if strings.HasPrefix(c.path, "/api/v3") {
		if handleRest(c) {
			return c.resp
		}
	}
	if handleGit(c) {
		return c.resp
	}
	return c.errJSON(404, "Not Found")
}

func errText(r any) string {
	if e, ok := r.(error); ok {
		return e.Error()
	}
	return "Internal error"
}

// --- response helpers (the Ctx's send functions) ---

// jsonResp builds a JSON response with the request-id header.
func (c *ctx) jsonResp(status int, body []byte, extra [][2]string) *httpResponse {
	return &httpResponse{
		status:  status,
		headers: append([][2]string{{"x-github-request-id", c.store.nextReqID()}}, extra...),
		body:    body,
	}
}

func (c *ctx) errJSON(status int, message string) *httpResponse {
	w := &jw{}
	w.raw(`{"message":`)
	w.str(message)
	w.raw(`,"documentation_url":"https://example.invalid/docs"}`)
	return c.jsonResp(status, w.bytes(), nil)
}

// authorized mirrors the Node forge: a Bearer token must be present and
// non-empty; when the server token is empty any non-empty bearer passes.
func (c *ctx) authorized() bool {
	header := c.auth
	if !strings.HasPrefix(header, "Bearer ") {
		return false
	}
	presented := trimSpace(header[len("Bearer "):])
	if presented == "" {
		return false
	}
	if c.token == "" {
		return true
	}
	return presented == c.token
}

func (c *ctx) parseBody() (map[string]any, bool) {
	return parseJSONBody(c.body)
}

// pageParams mirrors the Node forge's clamping (Number() || default, then
// clamp 1..100 / >= 1).
func (c *ctx) pageParams() (perPage int, page int) {
	perPage = 30
	if raw, ok := c.query["per_page"]; ok && len(raw) > 0 {
		if f, ok := jsNumber(raw[0]); ok {
			if f == 0 {
				f = 30
			}
			perPage = int(math.Max(1, math.Min(100, f)))
		}
	}
	page = 1
	if raw, ok := c.query["page"]; ok && len(raw) > 0 {
		if f, ok := jsNumber(raw[0]); ok {
			if f == 0 {
				f = 1
			}
			page = int(math.Max(1, f))
		}
	}
	return perPage, page
}

// paginate slices items and builds the Link header value, mirroring Node.
func (c *ctx) paginate(total int, basePath string) (start, end int, link string) {
	perPage, page := c.pageParams()
	start = (page - 1) * perPage
	end = start + perPage
	if end > total {
		end = total
	}
	if start > total {
		start = total
		end = total
	}
	if start+perPage < total {
		next := c.origin + basePath + "?per_page=" + strconv.Itoa(perPage) + "&page=" + strconv.Itoa(page+1)
		link = "<" + next + `>; rel="next"`
		if page > 1 {
			prev := c.origin + basePath + "?per_page=" + strconv.Itoa(perPage) + "&page=" + strconv.Itoa(page-1)
			link += ", <" + prev + `>; rel="prev"`
		}
	} else if page > 1 {
		prev := c.origin + basePath + "?per_page=" + strconv.Itoa(perPage) + "&page=" + strconv.Itoa(page-1)
		link = "<" + prev + `>; rel="prev"`
	}
	return start, end, link
}

func sortIssuesList(list []*issue, sortField, direction string) []*issue {
	out := append([]*issue(nil), list...)
	if sortField == "created" || sortField == "" {
		if direction == "desc" {
			sort.Slice(out, func(i, j int) bool { return out[i].number > out[j].number })
		} else {
			sort.Slice(out, func(i, j int) bool { return out[i].number < out[j].number })
		}
	}
	return out
}

// repoOr404 mirrors the Node forge: only the viewer's repos exist.
func (c *ctx) repoOr404(owner, name string) *repo {
	if owner != viewerLogin {
		return nil
	}
	return c.store.getRepo(owner, name)
}

// splitPath splits a decoded path into non-empty segments.
func splitPath(path string) []string {
	parts := strings.Split(path, "/")
	out := parts[:0]
	for _, p := range parts {
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

// decodeSeg decodes one path segment a second time (the Node forge applies
// decodeURIComponent to the whole pathname and then again per segment).
func decodeSeg(s string) (string, error) {
	return pathUnescape(s)
}

// bodyString extracts a string field from a parsed JSON body, or "".
func bodyString(body map[string]any, key string) string {
	if v, ok := body[key]; ok {
		if s, ok := v.(string); ok {
			return s
		}
	}
	return ""
}

// bodyBool extracts a boolean field, or false.
func bodyBool(body map[string]any, key string) bool {
	if v, ok := body[key]; ok {
		if b, ok := v.(bool); ok {
			return b
		}
	}
	return false
}

// bodyStringList extracts a string array field, or nil.
func bodyStringList(body map[string]any, key string) []string {
	v, ok := body[key]
	if !ok {
		return nil
	}
	arr, ok := v.([]any)
	if !ok {
		return nil
	}
	out := make([]string, 0, len(arr))
	for _, item := range arr {
		if s, ok := item.(string); ok {
			out = append(out, s)
		}
	}
	return out
}
