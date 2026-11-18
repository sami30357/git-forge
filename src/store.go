package main

// In-memory store and JSON serializers, ported from the Node forge with
// identical response shapes and deterministic counters.

import (
	"sort"
	"time"
)

const viewerLogin = "adaiversen"
const viewerID = 1000
const viewerName = "ada iversen"
const viewerEmail = "adaiversen@example.invalid"
const baseTS = int64(1735689600) // 2025-01-01T00:00:00Z, the scenario epoch
const ident = "ada iversen <adaiversen@example.invalid>"
const serverName = "forge"
const serverVersion = "1.0.0"

type label struct {
	id    int
	name  string
	color string
}

type comment struct {
	id        int
	body      string
	createdAt string
	updatedAt string
}

type issue struct {
	number    int
	id        int
	title     string
	body      string
	state     string // open | closed
	labels    []string
	createdAt string
	updatedAt string
	closedAt  string // "" = null
	hasClosed bool
	comments  []*comment
	cached    []byte // serialized issue JSON, nil when dirty
}

type pullRequest struct {
	number        int
	id            int
	title         string
	body          string
	state         string // open | closed
	headRef       string
	baseRef       string
	headSha       string
	baseSha       string
	merged        bool
	mergedAt      string
	mergeCommit   string
	createdAt     string
	updatedAt     string
	closedAt      string
	hasClosed     bool
}

type repo struct {
	id            int
	name          string
	owner         string
	isPrivate     bool
	description   string
	defaultBranch string
	createdAt     string
	updatedAt     string
	pushedAt      string
	refs          map[string]string
	refOrder      []string // insertion order of refs (REST /git/refs lists in this order)
	issues        map[int]*issue
	issueCounter  int
	pulls         map[int]*pullRequest
	pullCounter   int
	labels        map[string]*label
	labelCounter  int
	gen           uint64 // bumped on every ref change; invalidates adv/ls-refs caches
	advGen        uint64
	advUp         []byte // cached v0 advertisement (upload-pack)
	advRecv       []byte // cached v0 advertisement (receive-pack)
	lsGen         uint64
	lsCache       map[string][]byte
}

type store struct {
	objects          map[string]gitObject
	repos            map[string]*repo
	repoCounter      int
	commentIDCounter int
	tsCounter        int64
	reqCounter       int64
	gen              uint64 // bumped on every object put; invalidates fetch cache
	fetchCache       map[fetchKey][]byte
}

type fetchKey struct {
	repoID int
	gen    uint64
	body   string
}

func newStore() *store {
	return &store{
		objects:    make(map[string]gitObject),
		repos:      make(map[string]*repo),
		fetchCache: make(map[fetchKey][]byte),
	}
}

func (s *store) lookup(oid string) *gitObject {
	if obj, ok := s.objects[oid]; ok {
		o := obj
		return &o
	}
	return nil
}

func (s *store) lookupFunc() lookupFunc {
	return func(oid string) *gitObject { return s.lookup(oid) }
}

func (s *store) putObject(typ gitObjectType, data []byte) string {
	oid := hashObject(typ, data)
	if _, ok := s.objects[oid]; !ok {
		s.objects[oid] = gitObject{typ: typ, data: data}
	}
	s.gen++
	return oid
}

func (s *store) nextTS() int64 {
	s.tsCounter++
	return baseTS + s.tsCounter
}

func (s *store) nextReqID() string {
	s.reqCounter++
	n := s.reqCounter
	const digits = "0123456789abcdef"
	var buf [8]byte
	for i := 7; i >= 0; i-- {
		buf[i] = digits[n&0xf]
		n >>= 4
	}
	return "req-" + string(buf[:])
}

func repoKey(owner, name string) string { return owner + "/" + name }

func (s *store) getRepo(owner, name string) *repo {
	return s.repos[repoKey(owner, name)]
}

func (s *store) getRepoByID(id int) *repo {
	for _, r := range s.repos {
		if r.id == id {
			return r
		}
	}
	return nil
}

func (s *store) createRepo(owner, name string, isPrivate bool, description string) *repo {
	s.repoCounter++
	ts := s.nextTS()
	r := &repo{
		id:            s.repoCounter,
		name:          name,
		owner:         owner,
		isPrivate:     isPrivate,
		description:   description,
		defaultBranch: "main",
		createdAt:     isoOf(ts),
		updatedAt:     isoOf(ts),
		pushedAt:      isoOf(ts),
		refs:          make(map[string]string),
		issues:        make(map[int]*issue),
		pulls:         make(map[int]*pullRequest),
		labels:        make(map[string]*label),
		lsCache:       make(map[string][]byte),
	}
	s.repos[repoKey(owner, name)] = r
	return r
}

func (s *store) deleteRepo(owner, name string) {
	delete(s.repos, repoKey(owner, name))
}

// setRef sets a ref, appending to the insertion order when new.
func (r *repo) setRef(name, oid string) {
	if _, ok := r.refs[name]; !ok {
		r.refOrder = append(r.refOrder, name)
	}
	r.refs[name] = oid
}

// delRef deletes a ref and removes it from the insertion order.
func (r *repo) delRef(name string) {
	if _, ok := r.refs[name]; !ok {
		return
	}
	delete(r.refs, name)
	for i, n := range r.refOrder {
		if n == name {
			r.refOrder = append(r.refOrder[:i], r.refOrder[i+1:]...)
			break
		}
	}
}

// bumpRefs records a ref-map change on a repo (invalidates adv/ls-refs caches).
func (r *repo) bumpRefs() { r.gen++ }

// commitOn creates a commit on a branch with the given tree and parents.
func (s *store) commitOn(r *repo, branch, treeOID string, parents []string, message string) string {
	ts := s.nextTS()
	oid := s.putObject(objCommit, buildCommit(treeOID, parents, message, ts))
	r.setRef("refs/heads/"+branch, oid)
	r.bumpRefs()
	r.updatedAt = isoOf(ts)
	r.pushedAt = isoOf(ts)
	return oid
}

func (s *store) createIssue(r *repo, title, body string, labels []string) *issue {
	r.issueCounter++
	ts := s.nextTS()
	iss := &issue{
		number:    r.issueCounter,
		id:        r.issueCounter,
		title:     title,
		body:      body,
		state:     "open",
		labels:    append([]string(nil), labels...),
		createdAt: isoOf(ts),
		updatedAt: isoOf(ts),
	}
	r.issues[iss.number] = iss
	return iss
}

func (s *store) createPullRequest(r *repo, title, body, headRef, baseRef string) *pullRequest {
	headSha, ok1 := r.refs["refs/heads/"+headRef]
	baseSha, ok2 := r.refs["refs/heads/"+baseRef]
	if !ok1 || !ok2 {
		return nil
	}
	r.pullCounter++
	ts := s.nextTS()
	pr := &pullRequest{
		number:    r.pullCounter,
		id:        r.pullCounter,
		title:     title,
		body:      body,
		state:     "open",
		headRef:   headRef,
		baseRef:   baseRef,
		headSha:   headSha,
		baseSha:   baseSha,
		createdAt: isoOf(ts),
		updatedAt: isoOf(ts),
	}
	r.pulls[pr.number] = pr
	return pr
}

func (s *store) addComment(r *repo, iss *issue, body string) *comment {
	s.commentIDCounter++
	ts := s.nextTS()
	c := &comment{
		id:        s.commentIDCounter,
		body:      body,
		createdAt: isoOf(ts),
		updatedAt: isoOf(ts),
	}
	iss.comments = append(iss.comments, c)
	iss.cached = nil // comments count changed
	return c
}

func isoOf(ts int64) string {
	return time.Unix(ts, 0).UTC().Format("2006-01-02T15:04:05.000Z")
}

func gid(parts ...any) string {
	s := ""
	for i, p := range parts {
		if i > 0 {
			s += ":"
		}
		switch v := p.(type) {
		case string:
			s += v
		case int:
			s += itoa(int64(v))
		}
	}
	return b64encode(s)
}

func b64encode(s string) string {
	const enc = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
	var out []byte
	data := []byte(s)
	for i := 0; i < len(data); i += 3 {
		var n uint32
		rem := len(data) - i
		n = uint32(data[i]) << 16
		if rem > 1 {
			n |= uint32(data[i+1]) << 8
		}
		if rem > 2 {
			n |= uint32(data[i+2])
		}
		out = append(out, enc[(n>>18)&63], enc[(n>>12)&63])
		if rem > 1 {
			out = append(out, enc[(n>>6)&63])
		} else {
			out = append(out, '=')
		}
		if rem > 2 {
			out = append(out, enc[n&63])
		} else {
			out = append(out, '=')
		}
	}
	return string(out)
}

func b64decode(s string) []byte {
	// strip whitespace like Node's Buffer.from(s, 'base64')
	clean := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c != ' ' && c != '\t' && c != '\n' && c != '\r' {
			clean = append(clean, c)
		}
	}
	const rev = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
	var vals [256]int
	for i := range vals {
		vals[i] = -1
	}
	for i := 0; i < len(rev); i++ {
		vals[rev[i]] = i
	}
	var out []byte
	for i := 0; i+3 < len(clean)+1 && i+4 <= len(clean); i += 4 {
		var n uint32
		pad := 0
		for j := 0; j < 4; j++ {
			c := clean[i+j]
			if c == '=' {
				pad++
				n <<= 6
			} else {
				v := vals[c]
				if v < 0 {
					return out
				}
				n = (n << 6) | uint32(v)
			}
		}
		out = append(out, byte(n>>16))
		if pad < 2 {
			out = append(out, byte(n>>8))
		}
		if pad < 1 {
			out = append(out, byte(n))
		}
		if pad > 0 {
			break
		}
	}
	return out
}

// ---------------------------------------------------------------------------
// JSON serializers (hand-rolled, field order matches the Node forge)
// ---------------------------------------------------------------------------

func userJSON() []byte {
	w := &jw{}
	w.raw(`{"login":`)
	w.str(viewerLogin)
	w.raw(`,"id":1000,"node_id":`)
	w.str(gid("User", viewerID))
	w.raw(`,"name":`)
	w.str(viewerName)
	w.raw(`,"email":`)
	w.str(viewerEmail)
	w.raw(`,"type":"User","site_admin":false,"avatar_url":`)
	w.str("https://example.invalid/avatar/" + viewerLogin)
	w.raw(`,"html_url":`)
	w.str("https://example.invalid/" + viewerLogin)
	w.raw(`,"url":`)
	w.str("/api/v3/users/" + viewerLogin)
	w.raw(`}`)
	return w.bytes()
}

var userJSONBytes = userJSON()

func repoJSON(r *repo) []byte {
	w := &jw{}
	w.raw(`{"id":`)
	w.int(r.id)
	w.raw(`,"node_id":`)
	w.str(gid("Repository", r.id))
	w.raw(`,"name":`)
	w.str(r.name)
	w.raw(`,"full_name":`)
	w.str(r.owner + "/" + r.name)
	w.raw(`,"owner":{"login":`)
	w.str(r.owner)
	w.raw(`,"id":1000,"type":"User"},"private":`)
	w.boolean(r.isPrivate)
	w.raw(`,"description":`)
	w.str(r.description)
	w.raw(`,"default_branch":`)
	w.str(r.defaultBranch)
	w.raw(`,"created_at":`)
	w.str(r.createdAt)
	w.raw(`,"updated_at":`)
	w.str(r.updatedAt)
	w.raw(`,"pushed_at":`)
	w.str(r.pushedAt)
	w.raw(`,"html_url":`)
	w.str("https://example.invalid/" + r.owner + "/" + r.name)
	w.raw(`,"url":`)
	w.str("/api/v3/repos/" + r.owner + "/" + r.name)
	w.raw(`,"fork":false,"has_issues":true,"has_projects":true,"has_wiki":true}`)
	return w.bytes()
}

func labelJSON(l *label) []byte {
	w := &jw{}
	w.raw(`{"id":`)
	w.int(l.id)
	w.raw(`,"node_id":`)
	w.str(gid("Label", l.id))
	w.raw(`,"name":`)
	w.str(l.name)
	w.raw(`,"color":`)
	w.str(l.color)
	w.raw(`,"description":""}`)
	return w.bytes()
}

func issueJSON(r *repo, iss *issue) []byte {
	if iss.cached != nil {
		return iss.cached
	}
	w := &jw{}
	w.raw(`{"id":`)
	w.int(iss.id)
	w.raw(`,"node_id":`)
	w.str(gid("Issue", r.id, iss.number))
	w.raw(`,"number":`)
	w.int(iss.number)
	w.raw(`,"title":`)
	w.str(iss.title)
	w.raw(`,"body":`)
	w.str(iss.body)
	w.raw(`,"state":`)
	w.str(iss.state)
	w.raw(`,"labels":[`)
	for i, name := range iss.labels {
		if i > 0 {
			w.raw(`,`)
		}
		if l, ok := r.labels[name]; ok {
			w.raw(string(labelJSON(l)))
		} else {
			w.raw(`{"id":0,"name":`)
			w.str(name)
			w.raw(`,"color":"000000","description":""}`)
		}
	}
	w.raw(`],"user":{"login":`)
	w.str(viewerLogin)
	w.raw(`,"id":1000,"type":"User"},"assignee":null,"assignees":[],"created_at":`)
	w.str(iss.createdAt)
	w.raw(`,"updated_at":`)
	w.str(iss.updatedAt)
	w.raw(`,"closed_at":`)
	if iss.hasClosed {
		w.str(iss.closedAt)
	} else {
		w.null()
	}
	w.raw(`,"comments":`)
	w.int(len(iss.comments))
	w.raw(`,"html_url":`)
	w.str("https://example.invalid/" + r.owner + "/" + r.name + "/issues/" + itoa(int64(iss.number)))
	w.raw(`,"url":`)
	w.str("/api/v3/repos/" + r.owner + "/" + r.name + "/issues/" + itoa(int64(iss.number)))
	w.raw(`}`)
	iss.cached = w.bytes()
	return iss.cached
}

func commentJSON(r *repo, iss *issue, c *comment) []byte {
	w := &jw{}
	w.raw(`{"id":`)
	w.int(c.id)
	w.raw(`,"node_id":`)
	w.str(gid("IssueComment", c.id))
	w.raw(`,"body":`)
	w.str(c.body)
	w.raw(`,"user":{"login":`)
	w.str(viewerLogin)
	w.raw(`,"id":1000,"type":"User"},"created_at":`)
	w.str(c.createdAt)
	w.raw(`,"updated_at":`)
	w.str(c.updatedAt)
	w.raw(`,"html_url":`)
	w.str("https://example.invalid/" + r.owner + "/" + r.name + "/issues/" + itoa(int64(iss.number)) + "#issuecomment-" + itoa(int64(c.id)))
	w.raw(`,"url":`)
	w.str("/api/v3/repos/" + r.owner + "/" + r.name + "/issues/" + itoa(int64(iss.number)) + "/comments/" + itoa(int64(c.id)))
	w.raw(`}`)
	return w.bytes()
}

func prMergeable(s *store, r *repo, pr *pullRequest) bool {
	if pr.state != "open" {
		return true
	}
	baseTip, ok1 := r.refs["refs/heads/"+pr.baseRef]
	headTip, ok2 := r.refs["refs/heads/"+pr.headRef]
	if !ok1 || !ok2 {
		return false
	}
	return !mergeConflicts(baseTip, headTip, s.lookupFunc())
}

func prJSON(s *store, r *repo, pr *pullRequest) []byte {
	headTip := pr.headSha
	if v, ok := r.refs["refs/heads/"+pr.headRef]; ok {
		headTip = v
	}
	baseTip := pr.baseSha
	if v, ok := r.refs["refs/heads/"+pr.baseRef]; ok {
		baseTip = v
	}
	mergeable := prMergeable(s, r, pr)
	w := &jw{}
	w.raw(`{"id":`)
	w.int(pr.id)
	w.raw(`,"node_id":`)
	w.str(gid("PullRequest", r.id, pr.number))
	w.raw(`,"number":`)
	w.int(pr.number)
	w.raw(`,"state":`)
	w.str(pr.state)
	w.raw(`,"title":`)
	w.str(pr.title)
	w.raw(`,"body":`)
	w.str(pr.body)
	w.raw(`,"merged":`)
	w.boolean(pr.merged)
	w.raw(`,"merged_at":`)
	if pr.mergedAt != "" {
		w.str(pr.mergedAt)
	} else {
		w.null()
	}
	w.raw(`,"merge_commit_sha":`)
	if pr.mergeCommit != "" {
		w.str(pr.mergeCommit)
	} else {
		w.null()
	}
	w.raw(`,"mergeable":`)
	w.boolean(mergeable)
	w.raw(`,"mergeable_state":`)
	if pr.state != "open" {
		w.str("unknown")
	} else if mergeable {
		w.str("clean")
	} else {
		w.str("dirty")
	}
	w.raw(`,"head":{"ref":`)
	w.str(pr.headRef)
	w.raw(`,"sha":`)
	w.str(headTip)
	w.raw(`,"label":`)
	w.str(r.owner + ":" + pr.headRef)
	w.raw(`},"base":{"ref":`)
	w.str(pr.baseRef)
	w.raw(`,"sha":`)
	w.str(baseTip)
	w.raw(`,"label":`)
	w.str(r.owner + ":" + pr.baseRef)
	w.raw(`},"user":{"login":`)
	w.str(viewerLogin)
	w.raw(`,"id":1000,"type":"User"},"created_at":`)
	w.str(pr.createdAt)
	w.raw(`,"updated_at":`)
	w.str(pr.updatedAt)
	w.raw(`,"closed_at":`)
	if pr.hasClosed {
		w.str(pr.closedAt)
	} else {
		w.null()
	}
	w.raw(`,"html_url":`)
	w.str("https://example.invalid/" + r.owner + "/" + r.name + "/pull/" + itoa(int64(pr.number)))
	w.raw(`,"url":`)
	w.str("/api/v3/repos/" + r.owner + "/" + r.name + "/pulls/" + itoa(int64(pr.number)))
	w.raw(`}`)
	return w.bytes()
}

// sortedIssues returns the repo's issues in number order.
func sortedIssues(r *repo) []*issue {
	out := make([]*issue, 0, len(r.issues))
	for _, iss := range r.issues {
		out = append(out, iss)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].number < out[j].number })
	return out
}

// sortedPulls returns the repo's pull requests in number order.
func sortedPulls(r *repo) []*pullRequest {
	out := make([]*pullRequest, 0, len(r.pulls))
	for _, pr := range r.pulls {
		out = append(out, pr)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].number < out[j].number })
	return out
}

// sortedRepos returns the store's repos in id order.
func sortedRepos(s *store) []*repo {
	out := make([]*repo, 0, len(s.repos))
	for _, r := range s.repos {
		out = append(out, r)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].id < out[j].id })
	return out
}
