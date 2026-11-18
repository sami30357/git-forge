package main

// MCP (JSON-RPC 2.0 over Streamable HTTP; plain JSON answers, no SSE),
// ported from the Node forge with identical tool definitions and behaviour.

import (
	"encoding/json"
	"strings"
)

// mcpToolsJSON is the exact tool list the Node forge serves (JSON.stringify
// of its MCP_TOOLS, field order preserved).
const mcpToolsJSON = `[{"name":"create_repo","description":"Create a repository owned by the authenticated user.","inputSchema":{"type":"object","properties":{"name":{"type":"string","description":"Repository name"},"description":{"type":"string","description":"Repository description"},"private":{"type":"boolean","description":"Whether the repository is private"}},"required":["name"]}},{"name":"get_repo","description":"Get a repository by owner and name.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"}},"required":["owner","repo"]}},{"name":"create_issue","description":"Create an issue in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"title":{"type":"string","description":"Issue title"},"body":{"type":"string","description":"Issue body"}},"required":["owner","repo","title"]}},{"name":"list_issues","description":"List issues in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"state":{"type":"string","description":"open, closed or all"}},"required":["owner","repo"]}},{"name":"create_pull_request","description":"Create a pull request in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"title":{"type":"string","description":"Pull request title"},"body":{"type":"string","description":"Pull request body"},"head":{"type":"string","description":"Head branch name"},"base":{"type":"string","description":"Base branch name"}},"required":["owner","repo","title","head","base"]}},{"name":"merge_pull_request","description":"Merge a pull request with a squash merge.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"pull_number":{"type":"integer","description":"Pull request number"}},"required":["owner","repo","pull_number"]}},{"name":"list_pull_requests","description":"List pull requests in a repository.","inputSchema":{"type":"object","properties":{"owner":{"type":"string","description":"Repository owner login"},"repo":{"type":"string","description":"Repository name"},"full_name":{"type":"string","description":"owner/repo"},"state":{"type":"string","description":"open, closed or all"}},"required":["owner","repo"]}}]`

var mcpToolNames = []string{"create_repo", "get_repo", "create_issue", "list_issues", "create_pull_request", "merge_pull_request", "list_pull_requests"}

var mcpToolRequired = map[string][]string{
	"create_repo":          {"name"},
	"get_repo":             {"owner", "repo"},
	"create_issue":         {"owner", "repo", "title"},
	"list_issues":          {"owner", "repo"},
	"create_pull_request":  {"owner", "repo", "title", "head", "base"},
	"merge_pull_request":   {"owner", "repo", "pull_number"},
	"list_pull_requests":   {"owner", "repo"},
}

func mcpRepoArgs(args map[string]any) (owner, repo string) {
	if s, ok := args["owner"].(string); ok {
		owner = s
	}
	if s, ok := args["repo"].(string); ok {
		repo = s
	}
	if (owner == "" || repo == "") {
		if full, ok := args["full_name"].(string); ok {
			if slash := strings.Index(full, "/"); slash > 0 {
				owner = full[:slash]
				repo = full[slash+1:]
			}
		}
	}
	return owner, repo
}

// mcpToolResult builds a tool result: content[0].text is the JSON string of
// the value, structuredContent is the value itself. The value is passed as
// pre-serialized JSON bytes.
func mcpToolResultJSON(valueJSON []byte) *jobj {
	out := newJobj()
	content := []any{}
	textObj := newJobj()
	textObj.set("type", "text")
	textObj.set("text", string(valueJSON))
	content = append(content, textObj)
	out.set("content", content)
	out.set("structuredContent", json.RawMessage(valueJSON))
	return out
}

func mcpToolError(message string) *jobj {
	out := newJobj()
	content := []any{}
	textObj := newJobj()
	textObj.set("type", "text")
	textObj.set("text", message)
	content = append(content, textObj)
	out.set("content", content)
	out.set("isError", true)
	return out
}

func mcpMissingArg(args map[string]any, tool string) string {
	for _, required := range mcpToolRequired[tool] {
		v := args[required]
		missing := v == nil || v == ""
		if missing {
			if (required == "owner" || required == "repo") {
				if full, ok := args["full_name"].(string); ok && strings.Contains(full, "/") {
					continue
				}
			}
			return required
		}
	}
	return ""
}

func runMcpTool(store *store, name string, args map[string]any) *jobj {
	known := false
	for _, n := range mcpToolNames {
		if n == name {
			known = true
			break
		}
	}
	if !known {
		return mcpToolError("unknown tool " + name)
	}
	if missing := mcpMissingArg(args, name); missing != "" {
		return mcpToolError("missing required argument: " + missing)
	}
	if name == "create_repo" {
		repoName, _ := args["name"].(string)
		if !nameRe.MatchString(repoName) || strings.HasSuffix(repoName, ".git") {
			return mcpToolError("invalid repository name")
		}
		if store.getRepo(viewerLogin, repoName) != nil {
			return mcpToolError("name already exists on this account")
		}
		description, _ := args["description"].(string)
		isPrivate, _ := args["private"].(bool)
		repo := store.createRepo(viewerLogin, repoName, isPrivate, description)
		return mcpToolResultJSON(repoJSON(repo))
	}
	owner, repoName := mcpRepoArgs(args)
	var repo *repo
	if owner == viewerLogin {
		repo = store.getRepo(owner, repoName)
	}
	if repo == nil {
		return mcpToolError("repository not found")
	}
	switch name {
	case "get_repo":
		return mcpToolResultJSON(repoJSON(repo))
	case "create_issue":
		title, _ := args["title"].(string)
		body, _ := args["body"].(string)
		iss := store.createIssue(repo, title, body, []string{})
		return mcpToolResultJSON(issueJSON(repo, iss))
	case "list_issues":
		state, _ := args["state"].(string)
		if state == "" {
			state = "open"
		}
		list := sortedIssues(repo)
		if state != "all" {
			filtered := list[:0]
			for _, iss := range list {
				if iss.state == state {
					filtered = append(filtered, iss)
				}
			}
			list = filtered
		}
		w := &jw{}
		w.raw(`[`)
		for i, iss := range list {
			if i > 0 {
				w.raw(`,`)
			}
			w.raw(string(issueJSON(repo, iss)))
		}
		w.raw(`]`)
		return mcpToolResultJSON(w.bytes())
	case "create_pull_request":
		head, _ := args["head"].(string)
		base, _ := args["base"].(string)
		if _, ok := repo.refs["refs/heads/"+head]; !ok {
			return mcpToolError("head or base branch does not exist")
		}
		if _, ok := repo.refs["refs/heads/"+base]; !ok {
			return mcpToolError("head or base branch does not exist")
		}
		title, _ := args["title"].(string)
		body, _ := args["body"].(string)
		pr := store.createPullRequest(repo, title, body, head, base)
		if pr == nil {
			return mcpToolError("could not create pull request")
		}
		return mcpToolResultJSON(prJSON(store, repo, pr))
	case "merge_pull_request":
		number := 0
		switch n := args["pull_number"].(type) {
		case float64:
			number = int(n)
		case int:
			number = n
		}
		pr := repo.pulls[number]
		if pr == nil {
			return mcpToolError("pull request not found")
		}
		merged, status := mergePullRequest(store, repo, pr)
		if status == "already" {
			return mcpToolError("pull request is already merged")
		}
		if status == "conflict" {
			return mcpToolError("merge conflict")
		}
		if status == "not-found" {
			return mcpToolError("pull request not found")
		}
		w := &jw{}
		w.raw(`{"merged":true,"sha":`)
		w.str(merged)
		w.raw(`,"message":"Pull request successfully merged"}`)
		return mcpToolResultJSON(w.bytes())
	case "list_pull_requests":
		state, _ := args["state"].(string)
		if state == "" {
			state = "open"
		}
		list := sortedPulls(repo)
		if state != "all" {
			filtered := list[:0]
			for _, pr := range list {
				if pr.state == state {
					filtered = append(filtered, pr)
				}
			}
			list = filtered
		}
		w := &jw{}
		w.raw(`[`)
		for i, pr := range list {
			if i > 0 {
				w.raw(`,`)
			}
			w.raw(string(prJSON(store, repo, pr)))
		}
		w.raw(`]`)
		return mcpToolResultJSON(w.bytes())
	}
	return mcpToolError("unknown tool " + name)
}

func handleMcp(c *ctx) bool {
	if c.method == "DELETE" {
		c.resp = &httpResponse{
			status:          200,
			headers:         [][2]string{{"x-github-request-id", c.store.nextReqID()}},
			noContentType:   true,
			noContentLength: false,
		}
		return true
	}
	if c.method != "POST" {
		c.resp = c.errJSON(404, "Not Found")
		return true
	}
	if !c.authorized() {
		c.resp = c.errJSON(401, "Requires authentication")
		return true
	}
	var body any
	if err := json.Unmarshal(c.body, &body); err != nil {
		c.resp = &httpResponse{
			status:  400,
			headers: [][2]string{{"x-github-request-id", c.store.nextReqID()}},
			body:    []byte(`{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}`),
		}
		return true
	}
	rpc, ok := body.(map[string]any)
	if !ok {
		c.resp = &httpResponse{
			status:  400,
			headers: [][2]string{{"x-github-request-id", c.store.nextReqID()}},
			body:    []byte(`{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}`),
		}
		return true
	}
	id, hasID := rpc["id"]
	method, _ := rpc["method"].(string)
	// A notification (no id) gets a 202 with no body.
	if !hasID {
		c.resp = &httpResponse{
			status:        202,
			headers:       [][2]string{{"x-github-request-id", c.store.nextReqID()}},
			noContentType: true,
		}
		return true
	}
	params, _ := rpc["params"].(map[string]any)
	if params == nil {
		params = make(map[string]any)
	}
	reply := func(value *jobj) {
		w := &jw{}
		w.raw(`{"jsonrpc":"2.0","id":`)
		writeJSONValue(w, id)
		w.raw(`,`)
		// value's keys are appended after id (result or error).
		for i, k := range value.keys {
			if i > 0 {
				w.raw(`,`)
			}
			w.str(k)
			w.raw(`:`)
			writeJSONValue(w, value.vals[k])
		}
		w.raw(`}`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
	}
	mcpErr := func(code int, message string) *jobj {
		out := newJobj()
		errObj := newJobj()
		errObj.set("code", code)
		errObj.set("message", message)
		out.set("error", errObj)
		return out
	}
	if method == "server/discover" {
		result := newJobj()
		result.set("supportedVersions", []any{"2026-07-28", "2025-11-25"})
		result.set("protocolVersion", "2026-07-28")
		caps := newJobj()
		caps.set("tools", newJobj())
		result.set("capabilities", caps)
		info := newJobj()
		info.set("name", serverName)
		info.set("version", serverVersion)
		result.set("serverInfo", info)
		meta := newJobj()
		metaInfo := newJobj()
		metaInfo.set("name", serverName)
		metaInfo.set("version", serverVersion)
		meta.set("io.modelcontextprotocol/serverInfo", metaInfo)
		result.set("_meta", meta)
		out := newJobj()
		out.set("result", result)
		reply(out)
		return true
	}
	if method == "initialize" {
		requested, _ := params["protocolVersion"].(string)
		chosen := "2026-07-28"
		switch requested {
		case "2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05":
			chosen = requested
		}
		result := newJobj()
		result.set("protocolVersion", chosen)
		caps := newJobj()
		caps.set("tools", newJobj())
		result.set("capabilities", caps)
		info := newJobj()
		info.set("name", serverName)
		info.set("version", serverVersion)
		result.set("serverInfo", info)
		out := newJobj()
		out.set("result", result)
		reply(out)
		return true
	}
	if method == "tools/list" {
		result := newJobj()
		result.set("tools", json.RawMessage(mcpToolsJSON))
		out := newJobj()
		out.set("result", result)
		reply(out)
		return true
	}
	if method == "tools/call" {
		toolName, _ := params["name"].(string)
		args, _ := params["arguments"].(map[string]any)
		if args == nil {
			args = make(map[string]any)
		}
		known := false
		for _, n := range mcpToolNames {
			if n == toolName {
				known = true
				break
			}
		}
		if !known {
			reply(mcpErr(-32601, "unknown tool "+toolName))
			return true
		}
		if missing := mcpMissingArg(args, toolName); missing != "" {
			reply(mcpErr(-32602, "missing required argument: "+missing))
			return true
		}
		out := newJobj()
		out.set("result", runMcpTool(c.store, toolName, args))
		reply(out)
		return true
	}
	reply(mcpErr(-32601, "method not found: "+method))
	return true
}
