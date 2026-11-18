package main

// REST v3 handlers, ported from the Node forge with identical routing,
// statuses, validation messages and response shapes.

import (
	"math"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

var nameRe = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`)

// mustDecode decodes one path segment; a failure panics so route()'s recover
// answers 500, exactly like the Node forge's decodeURIComponent throw.
func mustDecode(s string) string {
	out, err := pathUnescape(s)
	if err != nil {
		panic(err)
	}
	return out
}

// toIntNumber converts a JS Number() result to an int map key, or false when
// the value is not an integer (or is out of range, which can never be found).
func toIntNumber(f float64) (int, bool) {
	if math.IsNaN(f) || math.IsInf(f, 0) || math.Trunc(f) != f {
		return 0, false
	}
	if f > 1<<53 || f < -(1<<53) {
		return 0, false
	}
	return int(f), true
}

func handleRest(c *ctx) bool {
	seg := splitPath(c.path)
	if len(seg) < 2 || seg[0] != "api" || seg[1] != "v3" {
		return false
	}
	rest := seg[2:]
	method := c.method

	if !c.authorized() {
		c.resp = c.errJSON(401, "Requires authentication")
		return true
	}

	// GET /api/v3/
	if len(rest) == 0 {
		if method != "GET" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		c.resp = c.jsonResp(200, []byte(`{}`), [][2]string{
			{"x-oauth-scopes", "repo, read:org"},
			{"x-accepted-oauth-scopes", "repo"},
		})
		return true
	}

	// GET /api/v3/meta
	if len(rest) == 1 && rest[0] == "meta" {
		if method != "GET" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		c.resp = c.jsonResp(200, []byte(`{"installed_version":"1.0.0"}`), nil)
		return true
	}

	// GET /api/v3/user
	if len(rest) == 1 && rest[0] == "user" {
		if method != "GET" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		c.resp = c.jsonResp(200, userJSONBytes, nil)
		return true
	}

	// /api/v3/user/repos
	if len(rest) == 2 && rest[0] == "user" && rest[1] == "repos" {
		if method == "POST" {
			parsed, ok := c.parseBody()
			if !ok {
				c.resp = c.errJSON(400, "Problems parsing JSON")
				return true
			}
			name := bodyString(parsed, "name")
			if name == "" || !nameRe.MatchString(name) || strings.HasSuffix(name, ".git") {
				c.resp = c.errJSON(422, "name is required and must be a valid repository name")
				return true
			}
			if c.store.getRepo(viewerLogin, name) != nil {
				c.resp = c.errJSON(422, "name already exists on this account")
				return true
			}
			repo := c.store.createRepo(viewerLogin, name, bodyBool(parsed, "private"), bodyString(parsed, "description"))
			c.resp = c.jsonResp(201, repoJSON(repo), nil)
			return true
		}
		if method == "GET" {
			all := sortedRepos(c.store)
			filtered := all[:0]
			for _, r := range all {
				if r.owner == viewerLogin {
					filtered = append(filtered, r)
				}
			}
			start, end, link := c.paginate(len(filtered), "/api/v3/user/repos")
			var extra [][2]string
			if link != "" {
				extra = [][2]string{{"link", link}}
			}
			w := &jw{}
			w.raw(`[`)
			for i := start; i < end; i++ {
				if i > start {
					w.raw(`,`)
				}
				w.raw(string(repoJSON(filtered[i])))
			}
			w.raw(`]`)
			c.resp = c.jsonResp(200, w.bytes(), extra)
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	// /api/v3/repos/{owner}/{repo}[...]
	if len(rest) >= 3 && rest[0] == "repos" {
		owner := mustDecode(rest[1])
		name := mustDecode(rest[2])
		tail := rest[3:]
		repo := c.repoOr404(owner, name)
		if repo == nil {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		return handleRepoRest(c, repo, tail)
	}

	c.resp = c.errJSON(404, "Not Found")
	return true
}

func handleRepoRest(c *ctx, repo *repo, tail []string) bool {
	method := c.method
	store := c.store

	// /repos/{o}/{r} itself
	if len(tail) == 0 {
		if method == "GET" {
			c.resp = c.jsonResp(200, repoJSON(repo), nil)
			return true
		}
		if method == "PATCH" {
			parsed, ok := c.parseBody()
			if !ok {
				c.resp = c.errJSON(400, "Problems parsing JSON")
				return true
			}
			if v, ok := parsed["description"]; ok {
				if s, ok := v.(string); ok {
					repo.description = s
				}
			}
			if v, ok := parsed["private"]; ok {
				if b, ok := v.(bool); ok {
					repo.isPrivate = b
				}
			}
			if v, ok := parsed["default_branch"]; ok {
				if s, ok := v.(string); ok && nameRe.MatchString(s) {
					repo.defaultBranch = s
				}
			}
			repo.updatedAt = isoOf(store.nextTS())
			c.resp = c.jsonResp(200, repoJSON(repo), nil)
			return true
		}
		if method == "DELETE" {
			store.deleteRepo(repo.owner, repo.name)
			c.resp = &httpResponse{
				status:          204,
				headers:         [][2]string{{"x-github-request-id", store.nextReqID()}},
				noContentType:   true,
				noContentLength: true,
			}
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	head := tail[0]

	// /branches
	if head == "branches" && len(tail) == 1 {
		if method != "GET" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		type branch struct {
			name string
			sha  string
		}
		var branches []branch
		for ref, sha := range repo.refs {
			if strings.HasPrefix(ref, "refs/heads/") {
				branches = append(branches, branch{ref[len("refs/heads/"):], sha})
			}
		}
		sort.Slice(branches, func(i, j int) bool { return branches[i].name < branches[j].name })
		w := &jw{}
		w.raw(`[`)
		for i, b := range branches {
			if i > 0 {
				w.raw(`,`)
			}
			w.raw(`{"name":`)
			w.str(b.name)
			w.raw(`,"commit":{"sha":`)
			w.str(b.sha)
			w.raw(`,"url":`)
			w.str("/api/v3/repos/" + repo.owner + "/" + repo.name + "/commits/" + b.sha)
			w.raw(`},"protected":false}`)
		}
		w.raw(`]`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
		return true
	}

	// /commits
	if head == "commits" && len(tail) == 1 {
		if method != "GET" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		shaParam := queryGet(c.query, "sha")
		var tip string
		var tipOK bool
		if shaParam != "" {
			if isOID(shaParam) {
				tip, tipOK = shaParam, true
			} else if v, ok := repo.refs["refs/heads/"+shaParam]; ok {
				tip, tipOK = v, true
			}
		} else if v, ok := repo.refs["refs/heads/"+repo.defaultBranch]; ok {
			tip, tipOK = v, true
		}
		if !tipOK || store.lookup(tip) == nil {
			c.resp = c.jsonResp(200, []byte(`[]`), nil)
			return true
		}
		w := &jw{}
		w.raw(`[`)
		seen := make(map[string]bool)
		queue := []string{tip}
		first := true
		for len(queue) > 0 {
			oid := queue[0]
			queue = queue[1:]
			if seen[oid] {
				continue
			}
			seen[oid] = true
			obj := store.lookup(oid)
			if obj == nil || obj.typ != objCommit {
				continue
			}
			commit := parseCommit(obj.data)
			if !first {
				w.raw(`,`)
			}
			first = false
			w.raw(`{"sha":`)
			w.str(oid)
			w.raw(`,"commit":{"message":`)
			w.str(commit.message)
			w.raw(`,"author":{"name":`)
			w.str(viewerName)
			w.raw(`,"email":`)
			w.str(viewerEmail)
			w.raw(`,"date":`)
			w.str(isoOf(baseTS))
			w.raw(`},"committer":{"name":`)
			w.str(viewerName)
			w.raw(`,"email":`)
			w.str(viewerEmail)
			w.raw(`,"date":`)
			w.str(isoOf(baseTS))
			w.raw(`},"tree":{"sha":`)
			w.str(commit.tree)
			w.raw(`}},"url":`)
			w.str("/api/v3/repos/" + repo.owner + "/" + repo.name + "/commits/" + oid)
			w.raw(`}`)
			queue = append(queue, commit.parents...)
		}
		w.raw(`]`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
		return true
	}

	// /git/refs
	if head == "git" && len(tail) > 1 && tail[1] == "refs" {
		if method == "POST" && len(tail) == 2 {
			parsed, ok := c.parseBody()
			if !ok {
				c.resp = c.errJSON(400, "Problems parsing JSON")
				return true
			}
			ref := bodyString(parsed, "ref")
			sha := bodyString(parsed, "sha")
			if !strings.HasPrefix(ref, "refs/") || !isOID(sha) || store.lookup(sha) == nil {
				c.resp = c.errJSON(422, "Invalid ref or sha")
				return true
			}
			if _, exists := repo.refs[ref]; exists {
				c.resp = c.errJSON(422, "Reference already exists")
				return true
			}
			repo.setRef(ref, sha)
			repo.bumpRefs()
			w := &jw{}
			w.raw(`{"ref":`)
			w.str(ref)
			w.raw(`,"object":{"sha":`)
			w.str(sha)
			w.raw(`,"type":"commit"},"url":`)
			w.str("/api/v3/repos/" + repo.owner + "/" + repo.name + "/git/refs/" + ref)
			w.raw(`}`)
			c.resp = c.jsonResp(201, w.bytes(), nil)
			return true
		}
		if method == "GET" {
			if len(tail) == 2 {
				w := &jw{}
				w.raw(`[`)
				for i, ref := range repo.refOrder {
					if i > 0 {
						w.raw(`,`)
					}
					sha := repo.refs[ref]
					w.raw(`{"ref":`)
					w.str(ref)
					w.raw(`,"object":{"sha":`)
					w.str(sha)
					w.raw(`,"type":"commit"},"url":`)
					w.str("/api/v3/repos/" + repo.owner + "/" + repo.name + "/git/refs/" + ref)
					w.raw(`}`)
				}
				w.raw(`]`)
				c.resp = c.jsonResp(200, w.bytes(), nil)
				return true
			}
			refName := "refs/" + strings.Join(mapStrings(tail[2:], mustDecode), "/")
			sha, ok := repo.refs[refName]
			if !ok {
				c.resp = c.errJSON(404, "Not Found")
				return true
			}
			w := &jw{}
			w.raw(`{"ref":`)
			w.str(refName)
			w.raw(`,"object":{"sha":`)
			w.str(sha)
			w.raw(`,"type":"commit"},"url":`)
			w.str("/api/v3/repos/" + repo.owner + "/" + repo.name + "/git/refs/" + refName)
			w.raw(`}`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	// /contents/{path...}
	if head == "contents" {
		segments := mapStrings(tail[1:], mustDecode)
		return handleContents(c, repo, segments)
	}

	// /labels
	if head == "labels" {
		if method == "POST" && len(tail) == 1 {
			parsed, ok := c.parseBody()
			if !ok {
				c.resp = c.errJSON(400, "Problems parsing JSON")
				return true
			}
			name := bodyString(parsed, "name")
			color := bodyString(parsed, "color")
			if color == "" {
				color = "000000"
			}
			if name == "" {
				c.resp = c.errJSON(422, "name is required")
				return true
			}
			if _, exists := repo.labels[name]; exists {
				c.resp = c.errJSON(422, "already_exists")
				return true
			}
			repo.labelCounter++
			l := &label{id: repo.labelCounter, name: name, color: color}
			repo.labels[name] = l
			c.resp = c.jsonResp(201, labelJSON(l), nil)
			return true
		}
		if method == "GET" && len(tail) == 1 {
			labels := make([]*label, 0, len(repo.labels))
			for _, l := range repo.labels {
				labels = append(labels, l)
			}
			sort.Slice(labels, func(i, j int) bool { return labels[i].id < labels[j].id })
			w := &jw{}
			w.raw(`[`)
			for i, l := range labels {
				if i > 0 {
					w.raw(`,`)
				}
				w.raw(string(labelJSON(l)))
			}
			w.raw(`]`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	// /issues
	if head == "issues" {
		if len(tail) == 1 {
			if method == "GET" {
				state := queryGet(c.query, "state")
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
				list = sortIssuesList(list, queryGet(c.query, "sort"), queryGet(c.query, "direction"))
				start, end, link := c.paginate(len(list), "/api/v3/repos/"+repo.owner+"/"+repo.name+"/issues")
				var extra [][2]string
				if link != "" {
					extra = [][2]string{{"link", link}}
				}
				w := &jw{}
				w.raw(`[`)
				for i := start; i < end; i++ {
					if i > start {
						w.raw(`,`)
					}
					w.raw(string(issueJSON(repo, list[i])))
				}
				w.raw(`]`)
				c.resp = c.jsonResp(200, w.bytes(), extra)
				return true
			}
			if method == "POST" {
				parsed, ok := c.parseBody()
				if !ok {
					c.resp = c.errJSON(400, "Problems parsing JSON")
					return true
				}
				title := bodyString(parsed, "title")
				if title == "" {
					c.resp = c.errJSON(422, "title is required")
					return true
				}
				labels := bodyStringList(parsed, "labels")
				if labels == nil {
					labels = []string{}
				}
				for _, l := range labels {
					if _, exists := repo.labels[l]; !exists {
						c.resp = c.errJSON(422, "label '"+l+"' does not exist")
						return true
					}
				}
				iss := store.createIssue(repo, title, bodyString(parsed, "body"), labels)
				c.resp = c.jsonResp(201, issueJSON(repo, iss), nil)
				return true
			}
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		numF, okNum := jsNumber(tail[1])
		num, okInt := toIntNumber(numF)
		var iss *issue
		if okNum && okInt {
			iss = repo.issues[num]
		}
		if iss == nil {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		if len(tail) == 2 {
			if method == "GET" {
				c.resp = c.jsonResp(200, issueJSON(repo, iss), nil)
				return true
			}
			if method == "PATCH" {
				parsed, ok := c.parseBody()
				if !ok {
					c.resp = c.errJSON(400, "Problems parsing JSON")
					return true
				}
				if v, ok := parsed["state"]; ok {
					if s, ok := v.(string); ok && (s == "open" || s == "closed") {
						iss.state = s
						if s == "closed" {
							iss.closedAt = isoOf(store.nextTS())
							iss.hasClosed = true
						} else {
							iss.closedAt = ""
							iss.hasClosed = false
						}
					}
				}
				if v, ok := parsed["title"]; ok {
					if s, ok := v.(string); ok {
						iss.title = s
					}
				}
				if v, ok := parsed["body"]; ok {
					if s, ok := v.(string); ok {
						iss.body = s
					}
				}
				if v, ok := parsed["labels"]; ok {
					if arr, ok := v.([]any); ok {
						labels := make([]string, 0, len(arr))
						for _, item := range arr {
							if s, ok := item.(string); ok {
								labels = append(labels, s)
							}
						}
						for _, l := range labels {
							if _, exists := repo.labels[l]; !exists {
								c.resp = c.errJSON(422, "label '"+l+"' does not exist")
								return true
							}
						}
						iss.labels = labels
					}
				}
				iss.updatedAt = isoOf(store.nextTS())
				iss.cached = nil
				c.resp = c.jsonResp(200, issueJSON(repo, iss), nil)
				return true
			}
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		if len(tail) == 3 && tail[2] == "comments" {
			if method == "GET" {
				w := &jw{}
				w.raw(`[`)
				for i, cm := range iss.comments {
					if i > 0 {
						w.raw(`,`)
					}
					w.raw(string(commentJSON(repo, iss, cm)))
				}
				w.raw(`]`)
				c.resp = c.jsonResp(200, w.bytes(), nil)
				return true
			}
			if method == "POST" {
				parsed, ok := c.parseBody()
				if !ok {
					c.resp = c.errJSON(400, "Problems parsing JSON")
					return true
				}
				text := bodyString(parsed, "body")
				if text == "" {
					c.resp = c.errJSON(422, "body is required")
					return true
				}
				cm := store.addComment(repo, iss, text)
				c.resp = c.jsonResp(201, commentJSON(repo, iss, cm), nil)
				return true
			}
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	// /pulls
	if head == "pulls" {
		if len(tail) == 1 {
			if method == "GET" {
				state := queryGet(c.query, "state")
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
				start, end, link := c.paginate(len(list), "/api/v3/repos/"+repo.owner+"/"+repo.name+"/pulls")
				var extra [][2]string
				if link != "" {
					extra = [][2]string{{"link", link}}
				}
				w := &jw{}
				w.raw(`[`)
				for i := start; i < end; i++ {
					if i > start {
						w.raw(`,`)
					}
					w.raw(string(prJSON(store, repo, list[i])))
				}
				w.raw(`]`)
				c.resp = c.jsonResp(200, w.bytes(), extra)
				return true
			}
			if method == "POST" {
				parsed, ok := c.parseBody()
				if !ok {
					c.resp = c.errJSON(400, "Problems parsing JSON")
					return true
				}
				headRef := bodyString(parsed, "head")
				baseRef := bodyString(parsed, "base")
				title := bodyString(parsed, "title")
				if headRef == "" || baseRef == "" || title == "" {
					c.resp = c.errJSON(422, "head, base and title are required")
					return true
				}
				if _, ok := repo.refs["refs/heads/"+headRef]; !ok {
					c.resp = c.errJSON(422, "head or base branch does not exist")
					return true
				}
				if _, ok := repo.refs["refs/heads/"+baseRef]; !ok {
					c.resp = c.errJSON(422, "head or base branch does not exist")
					return true
				}
				pr := store.createPullRequest(repo, title, bodyString(parsed, "body"), headRef, baseRef)
				if pr == nil {
					c.resp = c.errJSON(422, "could not create pull request")
					return true
				}
				c.resp = c.jsonResp(201, prJSON(store, repo, pr), nil)
				return true
			}
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		numF, okNum := jsNumber(tail[1])
		num, okInt := toIntNumber(numF)
		var pr *pullRequest
		if okNum && okInt {
			pr = repo.pulls[num]
		}
		if pr == nil {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		if len(tail) == 2 {
			if method == "GET" {
				c.resp = c.jsonResp(200, prJSON(store, repo, pr), nil)
				return true
			}
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		if len(tail) == 3 && tail[2] == "files" {
			if method != "GET" {
				c.resp = c.errJSON(404, "Not Found")
				return true
			}
			baseTip := pr.baseSha
			if v, ok := repo.refs["refs/heads/"+pr.baseRef]; ok {
				baseTip = v
			}
			headTip := pr.headSha
			if v, ok := repo.refs["refs/heads/"+pr.headRef]; ok {
				headTip = v
			}
			files := diffTrees(baseTip, headTip, store.lookupFunc())
			w := &jw{}
			w.raw(`[`)
			for i, f := range files {
				if i > 0 {
					w.raw(`,`)
				}
				w.raw(`{"filename":`)
				w.str(f.filename)
				w.raw(`,"status":`)
				w.str(f.status)
				w.raw(`,"additions":`)
				w.int(f.additions)
				w.raw(`,"deletions":`)
				w.int(f.deletions)
				w.raw(`,"changes":`)
				w.int(f.changes)
				w.raw(`}`)
			}
			w.raw(`]`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		if len(tail) == 3 && tail[2] == "merge" {
			if method != "PUT" {
				c.resp = c.errJSON(404, "Not Found")
				return true
			}
			if _, ok := c.parseBody(); !ok {
				c.resp = c.errJSON(400, "Problems parsing JSON")
				return true
			}
			merged, status := mergePullRequest(store, repo, pr)
			if status == "not-found" {
				c.resp = c.errJSON(404, "Not Found")
				return true
			}
			if status == "already" {
				c.resp = c.errJSON(405, "Pull request is already merged")
				return true
			}
			if status == "conflict" {
				c.resp = c.errJSON(405, "Merge conflict")
				return true
			}
			w := &jw{}
			w.raw(`{"merged":true,"sha":`)
			w.str(merged)
			w.raw(`,"message":"Pull request successfully merged"}`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		c.resp = c.errJSON(404, "Not Found")
		return true
	}

	c.resp = c.errJSON(404, "Not Found")
	return true
}

// mergePullRequest squash-merges a PR. Returns (merge oid, "") on success, or
// ("", "already" | "conflict" | "not-found").
func mergePullRequest(store *store, repo *repo, pr *pullRequest) (string, string) {
	if pr.state != "open" {
		return "", "already"
	}
	baseTip, ok1 := repo.refs["refs/heads/"+pr.baseRef]
	headTip, ok2 := repo.refs["refs/heads/"+pr.headRef]
	if !ok1 || !ok2 {
		return "", "not-found"
	}
	if mergeConflicts(baseTip, headTip, store.lookupFunc()) {
		return "", "conflict"
	}
	headTree := ""
	if obj := store.lookup(headTip); obj != nil {
		headTree = parseCommit(obj.data).tree
	}
	ts := store.nextTS()
	message := "Merge pull request #" + strconv.Itoa(pr.number) + " from " + repo.owner + "/" + pr.headRef + "\n\n" + pr.title
	mergeOID := store.putObject(objCommit, buildCommit(headTree, []string{baseTip}, message, ts))
	repo.setRef("refs/heads/"+pr.baseRef, mergeOID)
	repo.bumpRefs()
	repo.updatedAt = isoOf(ts)
	repo.pushedAt = isoOf(ts)
	pr.state = "closed"
	pr.merged = true
	pr.mergedAt = isoOf(ts)
	pr.mergeCommit = mergeOID
	pr.closedAt = isoOf(ts)
	pr.hasClosed = true
	pr.updatedAt = isoOf(ts)
	pr.baseSha = mergeOID
	return mergeOID, ""
}

func handleContents(c *ctx, repo *repo, segments []string) bool {
	method := c.method
	store := c.store

	resolveRef := func() (string, bool) {
		ref := queryGet(c.query, "ref")
		if ref != "" {
			if isOID(ref) {
				if store.lookup(ref) != nil {
					return ref, true
				}
				return "", false
			}
			if v, ok := repo.refs["refs/heads/"+ref]; ok {
				return v, true
			}
			return "", false
		}
		if v, ok := repo.refs["refs/heads/"+repo.defaultBranch]; ok {
			return v, true
		}
		return "", false
	}

	if method == "GET" {
		tip, ok := resolveRef()
		if !ok {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		commitObj := store.lookup(tip)
		if commitObj == nil || commitObj.typ != objCommit {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		rootTree := parseCommit(commitObj.data).tree
		if len(segments) == 0 {
			entries := listDir(rootTree, []string{}, store.lookupFunc())
			w := &jw{}
			w.raw(`[`)
			for i, e := range entries {
				if i > 0 {
					w.raw(`,`)
				}
				size := 0
				if e.mode != "40000" {
					if obj := store.lookup(e.oid); obj != nil {
						size = len(obj.data)
					}
				}
				w.raw(`{"type":`)
				if e.mode == "40000" {
					w.str("dir")
				} else {
					w.str("file")
				}
				w.raw(`,"name":`)
				w.str(e.name)
				w.raw(`,"path":`)
				w.str(e.name)
				w.raw(`,"sha":`)
				w.str(e.oid)
				w.raw(`,"size":`)
				w.int(size)
				w.raw(`}`)
			}
			w.raw(`]`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		dir := listDir(rootTree, segments, store.lookupFunc())
		if dir != nil {
			prefix := strings.Join(segments, "/")
			w := &jw{}
			w.raw(`[`)
			for i, e := range dir {
				if i > 0 {
					w.raw(`,`)
				}
				size := 0
				if e.mode != "40000" {
					if obj := store.lookup(e.oid); obj != nil {
						size = len(obj.data)
					}
				}
				w.raw(`{"type":`)
				if e.mode == "40000" {
					w.str("dir")
				} else {
					w.str("file")
				}
				w.raw(`,"name":`)
				w.str(e.name)
				w.raw(`,"path":`)
				w.str(prefix + "/" + e.name)
				w.raw(`,"sha":`)
				w.str(e.oid)
				w.raw(`,"size":`)
				w.int(size)
				w.raw(`}`)
			}
			w.raw(`]`)
			c.resp = c.jsonResp(200, w.bytes(), nil)
			return true
		}
		blobOID := getPath(rootTree, segments, store.lookupFunc())
		if blobOID == "" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		blob := store.lookup(blobOID)
		if blob == nil {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		w := &jw{}
		w.raw(`{"type":"file","encoding":"base64","content":`)
		w.str(b64encode(string(blob.data)))
		w.raw(`,"sha":`)
		w.str(blobOID)
		w.raw(`,"size":`)
		w.int(len(blob.data))
		w.raw(`,"name":`)
		w.str(segments[len(segments)-1])
		w.raw(`,"path":`)
		w.str(strings.Join(segments, "/"))
		w.raw(`}`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
		return true
	}

	if method == "PUT" || method == "DELETE" {
		parsed, ok := c.parseBody()
		if !ok {
			c.resp = c.errJSON(400, "Problems parsing JSON")
			return true
		}
		branch := bodyString(parsed, "branch")
		if branch == "" {
			branch = repo.defaultBranch
		}
		tip, hasTip := repo.refs["refs/heads/"+branch]
		var rootTree string
		if hasTip {
			if obj := store.lookup(tip); obj != nil {
				rootTree = parseCommit(obj.data).tree
			}
		}

		if method == "PUT" {
			message := bodyString(parsed, "message")
			if message == "" {
				message = "Update file"
			}
			content := bodyString(parsed, "content")
			if content == "" {
				c.resp = c.errJSON(422, "content is required")
				return true
			}
			bytes := b64decode(content)
			existing := getPath(rootTree, segments, store.lookupFunc())
			sha := bodyString(parsed, "sha")
			if existing != "" {
				if sha == "" {
					c.resp = c.errJSON(422, "sha is required to update a file")
					return true
				}
				if sha != existing {
					c.resp = c.errJSON(409, "sha does not match the current file")
					return true
				}
			}
			blobOID := store.putObject(objBlob, bytes)
			newTree := setPath(rootTree, segments, blobOID, store.lookupFunc(), store.putObject)
			var parents []string
			if hasTip {
				parents = []string{tip}
			}
			commitOID := store.commitOn(repo, branch, newTree, parents, message)
			status := 201
			if existing != "" {
				status = 200
			}
			w := &jw{}
			w.raw(`{"content":{"name":`)
			w.str(segments[len(segments)-1])
			w.raw(`,"path":`)
			w.str(strings.Join(segments, "/"))
			w.raw(`,"sha":`)
			w.str(blobOID)
			w.raw(`,"size":`)
			w.int(len(bytes))
			w.raw(`,"encoding":"base64"},"commit":{"sha":`)
			w.str(commitOID)
			w.raw(`,"message":`)
			w.str(message)
			w.raw(`}}`)
			c.resp = c.jsonResp(status, w.bytes(), nil)
			return true
		}

		// DELETE
		message := bodyString(parsed, "message")
		if message == "" {
			message = "Delete file"
		}
		sha := bodyString(parsed, "sha")
		existing := getPath(rootTree, segments, store.lookupFunc())
		if existing == "" {
			c.resp = c.errJSON(404, "Not Found")
			return true
		}
		if sha == "" || sha != existing {
			c.resp = c.errJSON(409, "sha does not match the current file")
			return true
		}
		newTree := deletePath(rootTree, segments, store.lookupFunc(), store.putObject)
		var parents []string
		if hasTip {
			parents = []string{tip}
		}
		commitOID := store.commitOn(repo, branch, newTree, parents, message)
		w := &jw{}
		w.raw(`{"commit":{"sha":`)
		w.str(commitOID)
		w.raw(`,"message":`)
		w.str(message)
		w.raw(`}}`)
		c.resp = c.jsonResp(200, w.bytes(), nil)
		return true
	}

	c.resp = c.errJSON(404, "Not Found")
	return true
}

func mapStrings(in []string, f func(string) string) []string {
	out := make([]string, len(in))
	for i, s := range in {
		out[i] = f(s)
	}
	return out
}
