package main

// GraphQL v4: a small executor for exactly the documents the suite sends,
// ported from the Node forge with identical behaviour.

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
)

// --- tokenizer ---

type gqlToken struct {
	kind  string // spread, punct, string, int, float, name, eof
	value string
}

func tokenizeGql(src string) ([]gqlToken, error) {
	var tokens []gqlToken
	i := 0
	for i < len(src) {
		ch := src[i]
		switch {
		case ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == ',':
			i++
		case ch == '#':
			for i < len(src) && src[i] != '\n' {
				i++
			}
		case ch == '.' && i+3 <= len(src) && src[i:i+3] == "...":
			tokens = append(tokens, gqlToken{"spread", "..."})
			i += 3
		case strings.IndexByte("{}()[]:!=$@&|", ch) >= 0:
			tokens = append(tokens, gqlToken{"punct", string(ch)})
			i++
		case ch == '"':
			j := i + 1
			var out strings.Builder
			for j < len(src) && src[j] != '"' {
				if src[j] == '\\' {
					esc := byte(0)
					if j+1 < len(src) {
						esc = src[j+1]
					}
					switch esc {
					case 'n':
						out.WriteByte('\n')
					case 't':
						out.WriteByte('\t')
					case 'r':
						out.WriteByte('\r')
					case 'b':
						out.WriteByte('\b')
					case 'f':
						out.WriteByte('\f')
					case 'u':
						n, _ := strconv.ParseInt(src[j+2:j+6], 16, 32)
						out.WriteRune(rune(n))
						j += 4
					default:
						out.WriteByte(esc)
					}
					j += 2
				} else {
					out.WriteByte(src[j])
					j++
				}
			}
			tokens = append(tokens, gqlToken{"string", out.String()})
			i = j + 1
		case ch >= '0' && ch <= '9' || ch == '-':
			j := i
			for j < len(src) && (src[j] >= '0' && src[j] <= '9' || src[j] == '.' || src[j] == 'e' || src[j] == 'E' || src[j] == '+' || src[j] == '-') {
				j++
			}
			raw := src[i:j]
			kind := "int"
			if strings.ContainsAny(raw, ".eE") {
				kind = "float"
			}
			tokens = append(tokens, gqlToken{kind, raw})
			i = j
		case ch >= 'A' && ch <= 'Z' || ch >= 'a' && ch <= 'z' || ch == '_':
			j := i
			for j < len(src) && (src[j] >= 'A' && src[j] <= 'Z' || src[j] >= 'a' && src[j] <= 'z' || src[j] >= '0' && src[j] <= '9' || src[j] == '_') {
				j++
			}
			tokens = append(tokens, gqlToken{"name", src[i:j]})
			i = j
		default:
			return nil, fmt.Errorf("unexpected GraphQL character %q", string(ch))
		}
	}
	tokens = append(tokens, gqlToken{"eof", ""})
	return tokens, nil
}

// --- values ---

type gqlValue struct {
	kind   string // string, int, float, bool, null, enum, var, list, object
	str    string
	num    float64
	b      bool
	items  []gqlValue
	fields []gqlObjField
}

type gqlObjField struct {
	name  string
	value gqlValue
}

type gqlField struct {
	alias      string
	hasAlias   bool
	name       string
	args       map[string]gqlValue
	selections []gqlSelection
	hasSel     bool
}

type gqlSelection struct {
	isFragment bool
	typeCond   string
	fragSel    []gqlSelection
	field      *gqlField
}

type gqlOperation struct {
	kind    string // query | mutation
	name    string
	varDefs map[string]gqlValue
	hasDef  map[string]bool
	sels    []gqlSelection
}

// --- parser ---

type gqlParser struct {
	tokens []gqlToken
	pos    int
}

func (p *gqlParser) peek() gqlToken { return p.tokens[p.pos] }
func (p *gqlParser) next() gqlToken {
	t := p.tokens[p.pos]
	p.pos++
	return t
}

func (p *gqlParser) expect(kind, value string) (gqlToken, error) {
	t := p.next()
	if t.kind != kind || (value != "" && t.value != value) {
		return t, fmt.Errorf("expected %s %s, got %s %s", kind, value, t.kind, t.value)
	}
	return t, nil
}

func (p *gqlParser) expectName(value string) (string, error) {
	t, err := p.expect("name", "")
	if err != nil {
		return "", err
	}
	if value != "" && t.value != value {
		return "", fmt.Errorf("expected %s, got %s", value, t.value)
	}
	return t.value, nil
}

func (p *gqlParser) parseDocument() ([]gqlOperation, error) {
	var ops []gqlOperation
	for p.peek().kind != "eof" {
		op, err := p.parseOperation()
		if err != nil {
			return nil, err
		}
		ops = append(ops, op)
	}
	return ops, nil
}

func (p *gqlParser) parseOperation() (gqlOperation, error) {
	var op gqlOperation
	op.kind = "query"
	op.varDefs = make(map[string]gqlValue)
	op.hasDef = make(map[string]bool)
	tok := p.peek()
	if tok.kind == "name" && (tok.value == "query" || tok.value == "mutation") {
		op.kind = p.next().value
		if p.peek().kind == "name" {
			op.name = p.next().value
		}
	}
	if p.peek().kind == "punct" && p.peek().value == "(" {
		p.next()
		for !(p.peek().kind == "punct" && p.peek().value == ")") {
			if _, err := p.expect("punct", "$"); err != nil {
				return op, err
			}
			varName, err := p.expectName("")
			if err != nil {
				return op, err
			}
			if _, err := p.expect("punct", ":"); err != nil {
				return op, err
			}
			if err := p.parseType(); err != nil {
				return op, err
			}
			if p.peek().kind == "punct" && p.peek().value == "=" {
				p.next()
				def, err := p.parseValue()
				if err != nil {
					return op, err
				}
				op.varDefs[varName] = def
				op.hasDef[varName] = true
			}
		}
		if _, err := p.expect("punct", ")"); err != nil {
			return op, err
		}
	}
	sels, err := p.parseSelectionSet()
	if err != nil {
		return op, err
	}
	op.sels = sels
	return op, nil
}

func (p *gqlParser) parseType() error {
	if p.peek().kind == "punct" && p.peek().value == "[" {
		p.next()
		if err := p.parseType(); err != nil {
			return err
		}
		if _, err := p.expect("punct", "]"); err != nil {
			return err
		}
	} else {
		if _, err := p.expectName(""); err != nil {
			return err
		}
	}
	if p.peek().kind == "punct" && p.peek().value == "!" {
		p.next()
	}
	return nil
}

func (p *gqlParser) parseSelectionSet() ([]gqlSelection, error) {
	if _, err := p.expect("punct", "{"); err != nil {
		return nil, err
	}
	var sels []gqlSelection
	for !(p.peek().kind == "punct" && p.peek().value == "}") {
		sel, err := p.parseSelection()
		if err != nil {
			return nil, err
		}
		sels = append(sels, sel)
	}
	if _, err := p.expect("punct", "}"); err != nil {
		return nil, err
	}
	return sels, nil
}

func (p *gqlParser) parseSelection() (gqlSelection, error) {
	if p.peek().kind == "spread" {
		p.next()
		if _, err := p.expectName("on"); err != nil {
			return gqlSelection{}, err
		}
		typeCond, err := p.expectName("")
		if err != nil {
			return gqlSelection{}, err
		}
		sels, err := p.parseSelectionSet()
		if err != nil {
			return gqlSelection{}, err
		}
		return gqlSelection{isFragment: true, typeCond: typeCond, fragSel: sels}, nil
	}
	name, err := p.expectName("")
	if err != nil {
		return gqlSelection{}, err
	}
	f := &gqlField{name: name, args: make(map[string]gqlValue)}
	if p.peek().kind == "punct" && p.peek().value == ":" {
		p.next()
		f.alias = name
		f.hasAlias = true
		name2, err := p.expectName("")
		if err != nil {
			return gqlSelection{}, err
		}
		f.name = name2
	}
	if p.peek().kind == "punct" && p.peek().value == "(" {
		p.next()
		for !(p.peek().kind == "punct" && p.peek().value == ")") {
			argName, err := p.expectName("")
			if err != nil {
				return gqlSelection{}, err
			}
			if _, err := p.expect("punct", ":"); err != nil {
				return gqlSelection{}, err
			}
			val, err := p.parseValue()
			if err != nil {
				return gqlSelection{}, err
			}
			f.args[argName] = val
		}
		if _, err := p.expect("punct", ")"); err != nil {
			return gqlSelection{}, err
		}
	}
	if p.peek().kind == "punct" && p.peek().value == "{" {
		sels, err := p.parseSelectionSet()
		if err != nil {
			return gqlSelection{}, err
		}
		f.selections = sels
		f.hasSel = true
	}
	return gqlSelection{field: f}, nil
}

func (p *gqlParser) parseValue() (gqlValue, error) {
	tok := p.next()
	switch tok.kind {
	case "string":
		return gqlValue{kind: "string", str: tok.value}, nil
	case "int":
		f, _ := strconv.ParseFloat(tok.value, 64)
		return gqlValue{kind: "int", num: f}, nil
	case "float":
		f, _ := strconv.ParseFloat(tok.value, 64)
		return gqlValue{kind: "float", num: f}, nil
	case "name":
		switch tok.value {
		case "true":
			return gqlValue{kind: "bool", b: true}, nil
		case "false":
			return gqlValue{kind: "bool", b: false}, nil
		case "null":
			return gqlValue{kind: "null"}, nil
		}
		return gqlValue{kind: "enum", str: tok.value}, nil
	case "punct":
		switch tok.value {
		case "$":
			name, err := p.expectName("")
			if err != nil {
				return gqlValue{}, err
			}
			return gqlValue{kind: "var", str: name}, nil
		case "[":
			var items []gqlValue
			for !(p.peek().kind == "punct" && p.peek().value == "]") {
				item, err := p.parseValue()
				if err != nil {
					return gqlValue{}, err
				}
				items = append(items, item)
			}
			if _, err := p.expect("punct", "]"); err != nil {
				return gqlValue{}, err
			}
			return gqlValue{kind: "list", items: items}, nil
		case "{":
			var fields []gqlObjField
			for !(p.peek().kind == "punct" && p.peek().value == "}") {
				name, err := p.expectName("")
				if err != nil {
					return gqlValue{}, err
				}
				if _, err := p.expect("punct", ":"); err != nil {
					return gqlValue{}, err
				}
				val, err := p.parseValue()
				if err != nil {
					return gqlValue{}, err
				}
				fields = append(fields, gqlObjField{name, val})
			}
			if _, err := p.expect("punct", "}"); err != nil {
				return gqlValue{}, err
			}
			return gqlValue{kind: "object", fields: fields}, nil
		}
	}
	return gqlValue{}, fmt.Errorf("unexpected GraphQL value %s %s", tok.kind, tok.value)
}

func evalGqlValue(v gqlValue, variables map[string]any) any {
	switch v.kind {
	case "string", "int", "float", "bool":
		if v.kind == "string" {
			return v.str
		}
		if v.kind == "bool" {
			return v.b
		}
		return v.num
	case "null":
		return nil
	case "enum":
		return v.str
	case "var":
		if val, ok := variables[v.str]; ok {
			return val
		}
		return nil
	case "list":
		out := make([]any, 0, len(v.items))
		for _, item := range v.items {
			out = append(out, evalGqlValue(item, variables))
		}
		return out
	case "object":
		out := newJobj()
		for _, f := range v.fields {
			out.set(f.name, evalGqlValue(f.value, variables))
		}
		return out
	}
	return nil
}

// --- schema ---

var gqlFields = map[string][]string{
	"Query":                    {"viewer", "repository", "node", "__type"},
	"Mutation":                 {"createIssue", "closeIssue", "createPullRequest", "mergePullRequest", "addComment"},
	"CreateIssuePayload":       {"issue"},
	"CloseIssuePayload":        {"issue"},
	"CreatePullRequestPayload": {"pullRequest"},
	"MergePullRequestPayload":  {"pullRequest"},
	"AddCommentPayload":        {"commentEdge"},
	"Viewer":                   {"login", "id", "name", "email", "databaseId", "avatarUrl"},
	"Repository":               {"id", "databaseId", "name", "nameWithOwner", "owner", "defaultBranchRef", "viewerPermission", "hasIssuesEnabled", "issue", "issues", "pullRequests", "pullRequest"},
	"User":                     {"login", "id", "name", "email", "databaseId"},
	"Ref":                      {"name", "target"},
	"Issue":                    {"id", "number", "title", "body", "state", "url", "createdAt", "author"},
	"IssueConnection":          {"nodes", "edges", "pageInfo", "totalCount"},
	"IssueEdge":                {"node", "cursor"},
	"PullRequestEdge":          {"node", "cursor"},
	"PullRequestFileEdge":      {"node", "cursor"},
	"PageInfo":                 {"hasNextPage", "hasPreviousPage", "startCursor", "endCursor"},
	"PullRequest":              {"id", "number", "title", "body", "state", "headRefName", "baseRefName", "mergeable", "merged", "files", "headRef", "baseRef", "author"},
	"PullRequestConnection":    {"nodes", "edges", "pageInfo", "totalCount"},
	"PullRequestFileConnection": {"nodes", "edges", "pageInfo", "totalCount"},
	"PullRequestFile":          {"path", "additions", "deletions", "changeType"},
	"IssueComment":             {"id", "body", "author", "createdAt"},
	"IssueCommentEdge":         {"node", "cursor"},
	"IssueCommentConnection":   {"nodes", "edges", "pageInfo", "totalCount"},
	"__Type":                   {"kind", "name", "description", "fields", "enumValues", "inputFields", "interfaces", "possibleTypes", "ofType"},
	"__Field":                  {"name", "description", "args", "type", "isDeprecated", "deprecationReason"},
	"__InputValue":             {"name", "description", "type", "defaultValue"},
	"__EnumValue":              {"name", "description", "isDeprecated", "deprecationReason"},
}

var gqlEnums = map[string][]string{
	"SearchType":             {"ISSUE", "REPOSITORY", "USER", "CODE", "DISCUSSION", "COMMIT"},
	"IssueState":             {"OPEN", "CLOSED"},
	"MergeableState":         {"MERGEABLE", "CONFLICTING", "UNKNOWN"},
	"PullRequestState":       {"OPEN", "CLOSED", "MERGED"},
	"PullRequestMergeMethod": {"MERGE", "SQUASH", "REBASE"},
	"RepositoryPermission":   {"ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ"},
	"IssueOrderField":        {"CREATED_AT", "UPDATED_AT", "COMMENTS"},
	"OrderDirection":         {"ASC", "DESC"},
}

// --- objects ---

type gqlObj struct {
	typ     string
	m       map[string]any
	repo    *repo
	issue   *issue
	pr      *pullRequest
	comment *comment
}

func newGqlObj(typ string) *gqlObj { return &gqlObj{typ: typ, m: make(map[string]any)} }

func (o *gqlObj) set(key string, v any) { o.m[key] = v }
func (o *gqlObj) get(key string) any {
	if v, ok := o.m[key]; ok {
		return v
	}
	return nil
}

func decodeGlobalID(id string) (kind string, parts []string, ok bool) {
	text := string(b64decode(id))
	seg := strings.Split(text, ":")
	if len(seg) < 2 {
		return "", nil, false
	}
	return seg[0], seg[1:], true
}

func cursorOf(index int) string { return b64encode("cursor:" + itoa(int64(index))) }

func indexOfCursor(cursor string) int {
	if cursor == "" {
		return -1
	}
	text := string(b64decode(cursor))
	if !strings.HasPrefix(text, "cursor:") {
		return -1
	}
	n, err := strconv.Atoi(text[len("cursor:"):])
	if err != nil {
		return -1
	}
	return n
}

// --- executor ---

type gqlExecutor struct {
	store *store
}

func (e *gqlExecutor) execute(query string, variables map[string]any) []byte {
	tokens, err := tokenizeGql(query)
	var ops []gqlOperation
	if err == nil {
		ops, err = (&gqlParser{tokens: tokens}).parseDocument()
	}
	if err != nil {
		return gqlErrorResponse(err.Error())
	}
	if len(ops) == 0 {
		return gqlErrorResponse("empty document")
	}
	op := ops[0]
	var errors []string
	rootType := "Query"
	if op.kind == "mutation" {
		rootType = "Mutation"
	}
	data := newJobj()
	for _, sel := range op.sels {
		if sel.isFragment {
			continue
		}
		f := sel.field
		key := f.name
		if f.hasAlias {
			key = f.alias
		}
		val, errMsg := e.resolveRoot(rootType, f, variables, &errors)
		if errMsg != "" {
			errors = append(errors, errMsg)
			data.set(key, nil)
		} else {
			data.set(key, val)
		}
	}
	return gqlDataResponse(data, errors)
}

func gqlErrorResponse(msg string) []byte {
	w := &jw{}
	w.raw(`{"data":null,"errors":[{"message":`)
	w.str(msg)
	w.raw(`}]}`)
	return w.bytes()
}

func gqlDataResponse(data *jobj, errors []string) []byte {
	w := &jw{}
	w.raw(`{"data":`)
	writeJSONValue(w, data)
	if len(errors) > 0 {
		w.raw(`,"errors":[`)
		for i, msg := range errors {
			if i > 0 {
				w.raw(`,`)
			}
			w.raw(`{"message":`)
			w.str(msg)
			w.raw(`}`)
		}
		w.raw(`]`)
	}
	w.raw(`}`)
	return w.bytes()
}

func (e *gqlExecutor) evalArgs(f *gqlField, variables map[string]any) map[string]any {
	out := make(map[string]any)
	for name, v := range f.args {
		out[name] = evalGqlValue(v, variables)
	}
	return out
}

func (e *gqlExecutor) resolveRoot(rootType string, f *gqlField, variables map[string]any, errors *[]string) (any, string) {
	args := e.evalArgs(f, variables)
	if rootType == "Query" {
		switch f.name {
		case "viewer":
			return e.resolveFields("Viewer", e.viewerObj(), f.selections, variables, errors), ""
		case "repository":
			owner := strArg(args, "owner")
			name := strArg(args, "name")
			var repo *repo
			if owner == viewerLogin {
				repo = e.store.getRepo(owner, name)
			}
			if repo == nil {
				return nil, ""
			}
			return e.resolveFields("Repository", e.repoObj(repo), f.selections, variables, errors), ""
		case "node":
			id, _ := args["id"].(string)
			kind, parts, ok := decodeGlobalID(id)
			if !ok {
				return nil, ""
			}
			obj := e.nodeObj(kind, parts)
			if obj == nil {
				return nil, ""
			}
			return e.resolveFields(obj.typ, obj, f.selections, variables, errors), ""
		case "__type":
			name := strArg(args, "name")
			typeObj := e.typeObj(name)
			if typeObj == nil {
				return nil, ""
			}
			return e.resolveFields("__Type", typeObj, f.selections, variables, errors), ""
		}
	} else {
		mutation := e.resolveMutation(f.name, args)
		if mutation == errUnknownMutation {
			*errors = append(*errors, `Cannot query field "`+f.name+`" on type "Mutation".`)
			return nil, ""
		}
		if mutation == nil {
			return nil, ""
		}
		return e.resolveFields(mutation.typ, mutation, f.selections, variables, errors), ""
	}
	*errors = append(*errors, `Cannot query field "`+f.name+`" on type "`+rootType+`".`)
	return nil, ""
}

var errUnknownMutation = &gqlObj{typ: "__unknown__"}

func strArg(args map[string]any, key string) string {
	if v, ok := args[key]; ok {
		if s, ok := v.(string); ok {
			return s
		}
	}
	return ""
}

func numArg(args map[string]any, key string) int {
	if v, ok := args[key]; ok {
		switch n := v.(type) {
		case float64:
			return int(n)
		case int:
			return n
		}
	}
	return 0
}

func (e *gqlExecutor) viewerObj() *gqlObj {
	o := newGqlObj("Viewer")
	o.set("login", viewerLogin)
	o.set("id", itoa(int64(viewerID)))
	o.set("databaseId", viewerID)
	o.set("name", viewerName)
	o.set("email", viewerEmail)
	o.set("avatarUrl", "https://example.invalid/avatar/"+viewerLogin)
	return o
}

func (e *gqlExecutor) userObj() *gqlObj {
	o := newGqlObj("User")
	o.set("login", viewerLogin)
	o.set("id", itoa(int64(viewerID)))
	o.set("databaseId", viewerID)
	o.set("name", viewerName)
	o.set("email", viewerEmail)
	return o
}

func (e *gqlExecutor) repoObj(r *repo) *gqlObj {
	o := newGqlObj("Repository")
	o.repo = r
	o.set("id", gid("Repository", r.id))
	o.set("databaseId", r.id)
	o.set("name", r.name)
	o.set("nameWithOwner", r.owner+"/"+r.name)
	o.set("owner", e.userObj())
	defRef := newGqlObj("Ref")
	defRef.set("name", r.defaultBranch)
	target := newGqlObj("Commit")
	target.set("oid", r.refs["refs/heads/"+r.defaultBranch])
	defRef.set("target", target)
	o.set("defaultBranchRef", defRef)
	o.set("viewerPermission", "ADMIN")
	o.set("hasIssuesEnabled", true)
	return o
}

func (e *gqlExecutor) issueObj(r *repo, iss *issue) *gqlObj {
	o := newGqlObj("Issue")
	o.repo = r
	o.issue = iss
	o.set("id", gid("Issue", r.id, iss.number))
	o.set("number", iss.number)
	o.set("title", iss.title)
	o.set("body", iss.body)
	o.set("state", strings.ToUpper(iss.state))
	o.set("url", "https://example.invalid/"+r.owner+"/"+r.name+"/issues/"+itoa(int64(iss.number)))
	o.set("createdAt", iss.createdAt)
	o.set("author", e.userObj())
	return o
}

func (e *gqlExecutor) prObj(r *repo, pr *pullRequest) *gqlObj {
	o := newGqlObj("PullRequest")
	o.repo = r
	o.pr = pr
	baseTip := pr.baseSha
	if v, ok := r.refs["refs/heads/"+pr.baseRef]; ok {
		baseTip = v
	}
	headTip := pr.headSha
	if v, ok := r.refs["refs/heads/"+pr.headRef]; ok {
		headTip = v
	}
	state := strings.ToUpper(pr.state)
	if pr.merged {
		state = "MERGED"
	}
	mergeable := "CONFLICTING"
	if prMergeable(e.store, r, pr) {
		mergeable = "MERGEABLE"
	}
	o.set("id", gid("PullRequest", r.id, pr.number))
	o.set("number", pr.number)
	o.set("title", pr.title)
	o.set("body", pr.body)
	o.set("state", state)
	o.set("headRefName", pr.headRef)
	o.set("baseRefName", pr.baseRef)
	o.set("mergeable", mergeable)
	o.set("merged", pr.merged)
	headRef := newGqlObj("Ref")
	headRef.set("name", pr.headRef)
	headTarget := newGqlObj("Commit")
	headTarget.set("oid", headTip)
	headRef.set("target", headTarget)
	o.set("headRef", headRef)
	baseRef := newGqlObj("Ref")
	baseRef.set("name", pr.baseRef)
	baseTarget := newGqlObj("Commit")
	baseTarget.set("oid", baseTip)
	baseRef.set("target", baseTarget)
	o.set("baseRef", baseRef)
	o.set("author", e.userObj())
	return o
}

func (e *gqlExecutor) commentObj(c *comment) *gqlObj {
	o := newGqlObj("IssueComment")
	o.comment = c
	o.set("id", gid("IssueComment", c.id))
	o.set("body", c.body)
	o.set("createdAt", c.createdAt)
	o.set("author", e.userObj())
	return o
}

func (e *gqlExecutor) nodeObj(kind string, parts []string) *gqlObj {
	num := func(i int) int {
		if i >= len(parts) {
			return 0
		}
		n, _ := strconv.Atoi(parts[i])
		return n
	}
	switch kind {
	case "Repository":
		r := e.store.getRepoByID(num(0))
		if r == nil {
			return nil
		}
		return e.repoObj(r)
	case "Issue":
		r := e.store.getRepoByID(num(0))
		if r == nil {
			return nil
		}
		iss := r.issues[num(1)]
		if iss == nil {
			return nil
		}
		return e.issueObj(r, iss)
	case "PullRequest":
		r := e.store.getRepoByID(num(0))
		if r == nil {
			return nil
		}
		pr := r.pulls[num(1)]
		if pr == nil {
			return nil
		}
		return e.prObj(r, pr)
	case "IssueComment":
		for _, r := range e.store.repos {
			for _, iss := range r.issues {
				for _, c := range iss.comments {
					if c.id == num(0) {
						return e.commentObj(c)
					}
				}
			}
		}
		return nil
	}
	return nil
}

func (e *gqlExecutor) typeObj(name string) *gqlObj {
	if fields, ok := gqlFields[name]; ok {
		o := newGqlObj("__Type")
		o.set("kind", "OBJECT")
		o.set("name", name)
		o.set("fields", stringList(fields))
		o.set("enumValues", nil)
		return o
	}
	if enums, ok := gqlEnums[name]; ok {
		o := newGqlObj("__Type")
		o.set("kind", "ENUM")
		o.set("name", name)
		o.set("fields", nil)
		o.set("enumValues", stringList(enums))
		return o
	}
	return nil
}

func stringList(in []string) []any {
	out := make([]any, len(in))
	for i, s := range in {
		out[i] = s
	}
	return out
}

func (e *gqlExecutor) resolveMutation(name string, args map[string]any) *gqlObj {
	input, _ := args["input"].(map[string]any)
	if input == nil {
		if j, ok := args["input"].(*jobj); ok && j != nil {
			input = make(map[string]any)
			for _, k := range j.keys {
				v, _ := j.get(k)
				input[k] = v
			}
		} else {
			input = make(map[string]any)
		}
	}
	switch name {
	case "createIssue":
		id, _ := input["repositoryId"].(string)
		kind, parts, ok := decodeGlobalID(id)
		if !ok || kind != "Repository" {
			return nil
		}
		r := e.store.getRepoByID(atoiSafe(parts, 0))
		if r == nil {
			return nil
		}
		title, _ := input["title"].(string)
		body, _ := input["body"].(string)
		iss := e.store.createIssue(r, title, body, []string{})
		o := newGqlObj("CreateIssuePayload")
		o.set("issue", e.issueObj(r, iss))
		return o
	case "closeIssue":
		id, _ := input["issueId"].(string)
		kind, parts, ok := decodeGlobalID(id)
		if !ok || kind != "Issue" {
			return nil
		}
		r := e.store.getRepoByID(atoiSafe(parts, 0))
		iss := r.issues[atoiSafe(parts, 1)]
		if r == nil || iss == nil {
			return nil
		}
		iss.state = "closed"
		iss.closedAt = isoOf(e.store.nextTS())
		iss.hasClosed = true
		iss.cached = nil
		o := newGqlObj("CloseIssuePayload")
		o.set("issue", e.issueObj(r, iss))
		return o
	case "createPullRequest":
		id, _ := input["repositoryId"].(string)
		kind, parts, ok := decodeGlobalID(id)
		if !ok || kind != "Repository" {
			return nil
		}
		r := e.store.getRepoByID(atoiSafe(parts, 0))
		if r == nil {
			return nil
		}
		title, _ := input["title"].(string)
		body, _ := input["body"].(string)
		head, _ := input["headRefName"].(string)
		base, _ := input["baseRefName"].(string)
		pr := e.store.createPullRequest(r, title, body, head, base)
		if pr == nil {
			return nil
		}
		o := newGqlObj("CreatePullRequestPayload")
		o.set("pullRequest", e.prObj(r, pr))
		return o
	case "mergePullRequest":
		id, _ := input["pullRequestId"].(string)
		kind, parts, ok := decodeGlobalID(id)
		if !ok || kind != "PullRequest" {
			return nil
		}
		r := e.store.getRepoByID(atoiSafe(parts, 0))
		pr := r.pulls[atoiSafe(parts, 1)]
		if r == nil || pr == nil {
			return nil
		}
		merged, _ := mergePullRequest(e.store, r, pr)
		_ = merged
		o := newGqlObj("MergePullRequestPayload")
		o.set("pullRequest", e.prObj(r, pr))
		return o
	case "addComment":
		id, _ := input["subjectId"].(string)
		kind, parts, ok := decodeGlobalID(id)
		if !ok || kind != "Issue" {
			return nil
		}
		r := e.store.getRepoByID(atoiSafe(parts, 0))
		iss := r.issues[atoiSafe(parts, 1)]
		if r == nil || iss == nil {
			return nil
		}
		body, _ := input["body"].(string)
		c := e.store.addComment(r, iss, body)
		edge := newGqlObj("IssueCommentEdge")
		edge.set("node", e.commentObj(c))
		edge.set("cursor", cursorOf(len(iss.comments)-1))
		o := newGqlObj("AddCommentPayload")
		o.set("commentEdge", edge)
		return o
	}
	return errUnknownMutation
}

func atoiSafe(parts []string, i int) int {
	if i >= len(parts) {
		return 0
	}
	n, _ := strconv.Atoi(parts[i])
	return n
}

func (e *gqlExecutor) resolveFields(typeName string, obj *gqlObj, sels []gqlSelection, variables map[string]any, errors *[]string) *jobj {
	if sels == nil {
		return nil
	}
	out := newJobj()
	for _, sel := range sels {
		if sel.isFragment {
			if sel.typeCond == typeName || sel.typeCond == "Node" {
				nested := e.resolveFields(typeName, obj, sel.fragSel, variables, errors)
				if nested != nil {
					for _, k := range nested.keys {
						v, _ := nested.get(k)
						out.set(k, v)
					}
				}
			}
			continue
		}
		f := sel.field
		key := f.name
		if f.hasAlias {
			key = f.alias
		}
		allowed := false
		for _, n := range gqlFields[typeName] {
			if n == f.name {
				allowed = true
				break
			}
		}
		if !allowed {
			*errors = append(*errors, `Cannot query field "`+f.name+`" on type "`+typeName+`".`)
			out.set(key, nil)
			continue
		}
		out.set(key, e.resolveField(typeName, obj, f, variables, errors))
	}
	return out
}

func (e *gqlExecutor) resolveField(typeName string, obj *gqlObj, f *gqlField, variables map[string]any, errors *[]string) any {
	args := e.evalArgs(f, variables)
	sel := f.selections
	hasSel := f.hasSel
	_ = hasSel
	switch typeName {
	case "Viewer", "User":
		return obj.get(f.name)
	case "Ref":
		if f.name == "target" {
			target, _ := obj.get("target").(*gqlObj)
			if target == nil || sel == nil {
				return nil
			}
			out := newJobj()
			out.set("oid", target.get("oid"))
			return out
		}
		return obj.get(f.name)
	case "Repository":
		r := obj.repo
		switch f.name {
		case "owner":
			owner, _ := obj.get("owner").(*gqlObj)
			return e.resolveFields("User", owner, sel, variables, errors)
		case "defaultBranchRef":
			ref, _ := obj.get("defaultBranchRef").(*gqlObj)
			return e.resolveFields("Ref", ref, sel, variables, errors)
		case "issue":
			iss := r.issues[numArg(args, "number")]
			if iss == nil {
				return nil
			}
			return e.resolveFields("Issue", e.issueObj(r, iss), sel, variables, errors)
		case "issues":
			return e.issuesConnection(r, args, sel, variables, errors)
		case "pullRequests":
			return e.prsConnection(r, args, sel, variables, errors)
		case "pullRequest":
			pr := r.pulls[numArg(args, "number")]
			if pr == nil {
				return nil
			}
			return e.resolveFields("PullRequest", e.prObj(r, pr), sel, variables, errors)
		}
		return obj.get(f.name)
	case "Issue":
		return obj.get(f.name)
	case "PullRequest":
		r := obj.repo
		pr := obj.pr
		switch f.name {
		case "files":
			return e.filesConnection(r, pr, args, sel, variables, errors)
		case "headRef", "baseRef":
			ref, _ := obj.get(f.name).(*gqlObj)
			return e.resolveFields("Ref", ref, sel, variables, errors)
		case "author":
			author, _ := obj.get("author").(*gqlObj)
			return e.resolveFields("User", author, sel, variables, errors)
		}
		return obj.get(f.name)
	case "IssueConnection", "PullRequestConnection", "PullRequestFileConnection", "IssueCommentConnection":
		nodes, _ := obj.get("nodes").([]*gqlObj)
		switch f.name {
		case "nodes":
			out := make([]any, 0, len(nodes))
			for _, n := range nodes {
				out = append(out, e.resolveFields(n.typ, n, sel, variables, errors))
			}
			return out
		case "edges":
			out := make([]any, 0, len(nodes))
			for i, n := range nodes {
				edgeType := "PullRequestFileEdge"
				if n.typ == "Issue" {
					edgeType = "IssueEdge"
				} else if n.typ == "PullRequest" {
					edgeType = "PullRequestEdge"
				}
				edge := newGqlObj(edgeType)
				edge.set("node", n)
				edge.set("cursor", cursorOf(i))
				out = append(out, e.resolveFields(edgeType, edge, sel, variables, errors))
			}
			return out
		case "pageInfo":
			pi, _ := obj.get("pageInfo").(*gqlObj)
			return e.resolveFields("PageInfo", pi, sel, variables, errors)
		case "totalCount":
			return len(nodes)
		}
		return nil
	case "IssueEdge", "PullRequestEdge", "PullRequestFileEdge", "IssueCommentEdge":
		if f.name == "node" {
			node, _ := obj.get("node").(*gqlObj)
			return e.resolveFields(node.typ, node, sel, variables, errors)
		}
		return obj.get(f.name)
	case "PullRequestFile":
		return obj.get(f.name)
	case "IssueComment":
		if f.name == "author" {
			author, _ := obj.get("author").(*gqlObj)
			return e.resolveFields("User", author, sel, variables, errors)
		}
		return obj.get(f.name)
	case "__Type":
		switch f.name {
		case "fields":
			var names []any
			if v := obj.get("fields"); v != nil {
				names = v.([]any)
			}
			out := make([]any, 0, len(names))
			for _, n := range names {
				fieldObj := newGqlObj("__Field")
				fieldObj.set("name", n)
				fieldObj.set("description", nil)
				fieldObj.set("args", []any{})
				fieldObj.set("type", nil)
				fieldObj.set("isDeprecated", false)
				fieldObj.set("deprecationReason", nil)
				out = append(out, e.resolveFields("__Field", fieldObj, sel, variables, errors))
			}
			return out
		case "enumValues":
			var values []any
			if v := obj.get("enumValues"); v != nil {
				values = v.([]any)
			}
			out := make([]any, 0, len(values))
			for _, n := range values {
				enumObj := newGqlObj("__EnumValue")
				enumObj.set("name", n)
				enumObj.set("description", nil)
				enumObj.set("isDeprecated", false)
				enumObj.set("deprecationReason", nil)
				out = append(out, e.resolveFields("__EnumValue", enumObj, sel, variables, errors))
			}
			return out
		case "inputFields", "interfaces", "possibleTypes":
			return []any{}
		}
		return obj.get(f.name)
	case "__Field", "__EnumValue", "__InputValue":
		return obj.get(f.name)
	case "CreateIssuePayload", "CloseIssuePayload":
		if f.name == "issue" {
			iss, _ := obj.get("issue").(*gqlObj)
			return e.resolveFields("Issue", iss, sel, variables, errors)
		}
		return nil
	case "CreatePullRequestPayload", "MergePullRequestPayload":
		if f.name == "pullRequest" {
			pr, _ := obj.get("pullRequest").(*gqlObj)
			return e.resolveFields("PullRequest", pr, sel, variables, errors)
		}
		return nil
	case "AddCommentPayload":
		if f.name == "commentEdge" {
			edge, _ := obj.get("commentEdge").(*gqlObj)
			return e.resolveFields("IssueCommentEdge", edge, sel, variables, errors)
		}
		return nil
	}
	return obj.get(f.name)
}

func (e *gqlExecutor) issuesConnection(r *repo, args map[string]any, sel []gqlSelection, variables map[string]any, errors *[]string) *jobj {
	list := sortedIssues(r)
	if states, ok := args["states"].([]any); ok && len(states) > 0 {
		wanted := make(map[string]bool)
		for _, s := range states {
			if str, ok := s.(string); ok {
				wanted[strings.ToUpper(str)] = true
			}
		}
		filtered := list[:0]
		for _, iss := range list {
			if wanted[strings.ToUpper(iss.state)] {
				filtered = append(filtered, iss)
			}
		}
		list = filtered
	}
	if orderBy := gqlStrField(args["orderBy"], "field"); orderBy == "CREATED_AT" {
		if dir := gqlStrField(args["orderBy"], "direction"); dir == "DESC" {
			for i, j := 0, len(list)-1; i < j; i, j = i+1, j-1 {
				list[i], list[j] = list[j], list[i]
			}
		}
	}
	nodes := make([]*gqlObj, 0, len(list))
	for _, iss := range list {
		nodes = append(nodes, e.issueObj(r, iss))
	}
	return e.connection(nodes, args, sel, variables, errors)
}

func gqlStrField(v any, key string) string {
	switch t := v.(type) {
	case *jobj:
		if val, ok := t.get(key); ok {
			if s, ok := val.(string); ok {
				return s
			}
		}
	case map[string]any:
		if s, ok := t[key].(string); ok {
			return s
		}
	}
	return ""
}

func (e *gqlExecutor) prsConnection(r *repo, args map[string]any, sel []gqlSelection, variables map[string]any, errors *[]string) *jobj {
	list := sortedPulls(r)
	nodes := make([]*gqlObj, 0, len(list))
	for _, pr := range list {
		nodes = append(nodes, e.prObj(r, pr))
	}
	return e.connection(nodes, args, sel, variables, errors)
}

func (e *gqlExecutor) filesConnection(r *repo, pr *pullRequest, args map[string]any, sel []gqlSelection, variables map[string]any, errors *[]string) *jobj {
	baseTip := pr.baseSha
	if v, ok := r.refs["refs/heads/"+pr.baseRef]; ok {
		baseTip = v
	}
	headTip := pr.headSha
	if v, ok := r.refs["refs/heads/"+pr.headRef]; ok {
		headTip = v
	}
	files := diffTrees(baseTip, headTip, e.store.lookupFunc())
	nodes := make([]*gqlObj, 0, len(files))
	for _, f := range files {
		o := newGqlObj("PullRequestFile")
		o.set("path", f.filename)
		o.set("additions", f.additions)
		o.set("deletions", f.deletions)
		o.set("changeType", strings.ToUpper(f.status))
		nodes = append(nodes, o)
	}
	return e.connection(nodes, args, sel, variables, errors)
}

func (e *gqlExecutor) connection(nodes []*gqlObj, args map[string]any, sel []gqlSelection, variables map[string]any, errors *[]string) *jobj {
	after := -1
	if s, ok := args["after"].(string); ok {
		after = indexOfCursor(s)
	}
	first := len(nodes)
	if f, ok := args["first"].(float64); ok {
		if f > float64(len(nodes)) {
			first = len(nodes)
		} else if f < 0 {
			first = 0
		} else {
			first = int(f)
		}
	}
	start := after + 1
	if start < 0 {
		start = 0
	}
	end := start + first
	if end > len(nodes) {
		end = len(nodes)
	}
	if start > len(nodes) {
		start = len(nodes)
		end = len(nodes)
	}
	page := nodes[start:end]
	var endCursor any
	if len(page) > 0 {
		endCursor = cursorOf(start + len(page) - 1)
	}
	var startCursor any
	if len(page) > 0 {
		startCursor = cursorOf(start)
	}
	pi := newGqlObj("PageInfo")
	pi.set("hasNextPage", start+len(page) < len(nodes))
	pi.set("hasPreviousPage", start > 0)
	pi.set("startCursor", startCursor)
	pi.set("endCursor", endCursor)
	conn := newGqlObj("IssueConnection")
	conn.set("nodes", page)
	conn.set("pageInfo", pi)
	return e.resolveFields("IssueConnection", conn, sel, variables, errors)
}

// --- HTTP handler ---

func handleGraphQL(c *ctx) bool {
	if !c.authorized() {
		c.resp = c.errJSON(401, "Requires authentication")
		return true
	}
	var body any
	if err := json.Unmarshal(c.body, &body); err != nil {
		c.resp = c.errJSON(400, "Problems parsing JSON")
		return true
	}
	executor := &gqlExecutor{store: c.store}
	runOne := func(doc any) []byte {
		m, ok := doc.(map[string]any)
		if !ok {
			return gqlErrorResponse("invalid document")
		}
		query, _ := m["query"].(string)
		variables, _ := m["variables"].(map[string]any)
		if variables == nil {
			variables = make(map[string]any)
		}
		return executor.execute(query, variables)
	}
	if arr, ok := body.([]any); ok {
		w := &jw{}
		w.raw(`[`)
		for i, doc := range arr {
			if i > 0 {
				w.raw(`,`)
			}
			w.raw(string(runOne(doc)))
		}
		w.raw(`]`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
		return true
	}
	c.resp = c.jsonResp(200, runOne(body), nil)
	return true
}
