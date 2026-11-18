package main

// Git smart HTTP (v2 + v0), ported from the Node forge with identical
// behaviour and responses. Caches are layered on top: the v2 advertisement
// is constant bytes, the v0 advertisement and ls-refs responses are cached
// per repo ref-generation, and fetch responses per store object-generation
// plus the raw request body.

import (
	"bytes"
	"sort"
	"strings"
)

// handleGit routes git smart-HTTP requests. Returns false when the path is
// not a git endpoint. Layout: /{owner}/{repo}.git/info/refs,
// /{owner}/{repo}.git/git-upload-pack, /{owner}/{repo}.git/git-receive-pack.
func handleGit(c *ctx) bool {
	seg := splitPath(c.path)
	if len(seg) < 2 || !strings.HasSuffix(seg[1], ".git") {
		return false
	}
	owner := mustDecode(seg[0])
	name := mustDecode(seg[1][:len(seg[1])-4])
	if !c.authorized() {
		c.resp = c.errJSON(401, "Requires authentication")
		return true
	}
	repo := c.repoOr404(owner, name)
	if repo == nil {
		c.resp = c.errJSON(404, "Not Found")
		return true
	}
	if c.method == "GET" && len(seg) == 3 && seg[2] == "info" {
		c.resp = c.errJSON(404, "Not Found")
		return true
	}
	if c.method == "GET" && len(seg) == 4 && seg[2] == "info" && seg[3] == "refs" {
		service := queryGet(c.query, "service")
		if service == "" {
			service = "git-upload-pack"
		}
		return handleInfoRefs(c, repo, service)
	}
	if c.method == "POST" && len(seg) == 3 && seg[2] == "git-upload-pack" {
		return handleUploadPack(c, repo, strings.HasPrefix(c.gitProtocol, "version=2"))
	}
	if c.method == "POST" && len(seg) == 3 && seg[2] == "git-receive-pack" {
		return handleReceivePack(c, repo)
	}
	c.resp = c.errJSON(404, "Not Found")
	return true
}

// gitResp builds a git response: the given content type, a request id, and
// the body (no cache-control, matching the Node forge's sendGit).
func gitResp(store *store, contentType string, body []byte) *httpResponse {
	return &httpResponse{
		status:      200,
		headers:     [][2]string{{"x-github-request-id", store.nextReqID()}},
		body:        body,
		contentType: contentType,
	}
}

// v2AdvertisementBytes is the constant protocol-v2 capability advertisement.
var v2AdvertisementBytes = func() []byte {
	var parts [][]byte
	for _, line := range []string{"version 2\n", "ls-refs=unborn\n", "fetch=shallow\n", "object-format=sha1\n"} {
		parts = append(parts, pktLine([]byte(line)))
	}
	parts = append(parts, pktFlush())
	return concatBytes(parts)
}()

func handleInfoRefs(c *ctx, repo *repo, service string) bool {
	store := c.store
	if service == "git-upload-pack" {
		if strings.HasPrefix(c.gitProtocol, "version=2") {
			c.resp = gitResp(store, "application/x-git-upload-pack-advertisement", v2AdvertisementBytes)
		} else {
			c.resp = gitResp(store, "application/x-git-upload-pack-advertisement", v0Advertisement(store, repo, service))
		}
		return true
	}
	if service == "git-receive-pack" {
		c.resp = gitResp(store, "application/x-git-receive-pack-advertisement", v0Advertisement(store, repo, service))
		return true
	}
	c.resp = c.errJSON(404, "Not Found")
	return true
}

// refEntries returns the repo's refs sorted by name (the Node forge's
// Object.keys sort order).
func refEntries(repo *repo) []struct {
	name string
	oid  string
} {
	names := make([]string, 0, len(repo.refs))
	for name := range repo.refs {
		names = append(names, name)
	}
	sort.Strings(names)
	out := make([]struct {
		name string
		oid  string
	}, 0, len(names))
	for _, name := range names {
		out = append(out, struct {
			name string
			oid  string
		}{name, repo.refs[name]})
	}
	return out
}

// peeledOf returns the direct target of an annotated tag, or "".
func peeledOf(store *store, oid string) string {
	obj := store.lookup(oid)
	if obj == nil || obj.typ != objTag {
		return ""
	}
	return parseTag(obj.data).object
}

// v0Advertisement builds the dumb-protocol v0 advertisement, cached per repo
// ref-generation (separately per service, since the caps differ).
func v0Advertisement(store *store, repo *repo, service string) []byte {
	if repo.advGen == repo.gen {
		if service == "git-upload-pack" && repo.advUp != nil {
			return repo.advUp
		}
		if service == "git-receive-pack" && repo.advRecv != nil {
			return repo.advRecv
		}
	}
	caps := "symref=HEAD:refs/heads/main side-band-64k shallow thin-pack ofs-delta multi_ack"
	if service == "git-receive-pack" {
		caps = "symref=HEAD:refs/heads/main report-status side-band-64k delete-refs"
	}
	var parts [][]byte
	parts = append(parts, pktLine([]byte("# service="+service+"\n")), pktFlush())
	refs := refEntries(repo)
	if len(refs) == 0 {
		parts = append(parts, pktLine([]byte(zeroOID+" capabilities^{}\x00"+caps+"\n")))
	} else {
		if mainTip, ok := repo.refs["refs/heads/"+repo.defaultBranch]; ok {
			parts = append(parts, pktLine([]byte(mainTip+" HEAD\x00"+caps+"\n")))
		} else {
			parts = append(parts, pktLine([]byte(refs[0].oid+" "+refs[0].name+"\x00"+caps+"\n")))
		}
		for _, r := range refs {
			parts = append(parts, pktLine([]byte(r.oid+" "+r.name+"\n")))
			if peeled := peeledOf(store, r.oid); peeled != "" {
				parts = append(parts, pktLine([]byte(peeled+" "+r.name+"^{}\n")))
			}
		}
	}
	parts = append(parts, pktFlush())
	body := concatBytes(parts)
	repo.advGen = repo.gen
	if service == "git-upload-pack" {
		repo.advUp = body
	} else {
		repo.advRecv = body
	}
	return body
}

func concatBytes(parts [][]byte) []byte {
	n := 0
	for _, p := range parts {
		n += len(p)
	}
	out := make([]byte, 0, n)
	for _, p := range parts {
		out = append(out, p...)
	}
	return out
}

// --- ls-refs ---

type lsRefsArgs struct {
	peel     bool
	symrefs  bool
	unborn   bool
	prefixes []string
}

func parseLsRefsArgs(packets []pktPacket) lsRefsArgs {
	var args lsRefsArgs
	seenDelim := false
	for _, p := range packets {
		if p.kind == 'd' {
			seenDelim = true
			continue
		}
		if p.kind != 'D' {
			continue
		}
		line := packetText(p)
		if !seenDelim {
			continue
		}
		switch {
		case line == "peel\n":
			args.peel = true
		case line == "symrefs\n":
			args.symrefs = true
		case line == "unborn\n":
			args.unborn = true
		case strings.HasPrefix(line, "ref-prefix "):
			args.prefixes = append(args.prefixes, strings.TrimSuffix(line[len("ref-prefix "):], "\n"))
		}
	}
	return args
}

func lsRefsKey(args lsRefsArgs) string {
	var sb strings.Builder
	if args.peel {
		sb.WriteString("peel;")
	}
	if args.symrefs {
		sb.WriteString("symrefs;")
	}
	if args.unborn {
		sb.WriteString("unborn;")
	}
	for _, p := range args.prefixes {
		sb.WriteString("prefix=")
		sb.WriteString(p)
		sb.WriteString(";")
	}
	return sb.String()
}

func lsRefsResponse(store *store, repo *repo, args lsRefsArgs) []byte {
	key := lsRefsKey(args)
	if repo.lsGen != repo.gen {
		repo.lsCache = make(map[string][]byte)
		repo.lsGen = repo.gen
	} else if cached, ok := repo.lsCache[key]; ok {
		return cached
	}
	keep := func(name string) bool {
		if len(args.prefixes) == 0 {
			return true
		}
		for _, p := range args.prefixes {
			if strings.HasPrefix(name, p) {
				return true
			}
		}
		return false
	}
	var lines []string
	if mainTip, ok := repo.refs["refs/heads/"+repo.defaultBranch]; ok {
		if keep("HEAD") {
			if args.symrefs {
				lines = append(lines, mainTip+" HEAD symref-target:refs/heads/"+repo.defaultBranch+"\n")
			} else {
				lines = append(lines, mainTip+" HEAD\n")
			}
		}
	} else if args.unborn {
		if keep("HEAD") {
			lines = append(lines, "unborn HEAD symref-target:refs/heads/"+repo.defaultBranch+"\n")
		}
	}
	for _, r := range refEntries(repo) {
		if !keep(r.name) {
			continue
		}
		line := r.oid + " " + r.name
		if args.peel {
			if peeled := peeledOf(store, r.oid); peeled != "" {
				line += " peeled:" + peeled
			}
		}
		lines = append(lines, line+"\n")
	}
	var parts [][]byte
	for _, l := range lines {
		parts = append(parts, pktLine([]byte(l)))
	}
	parts = append(parts, pktFlush())
	body := concatBytes(parts)
	repo.lsCache[key] = body
	return body
}

// --- fetch ---

type fetchArgs struct {
	wants   []string
	haves   []string
	shallow []string
	depth   int
	hasDepth bool
}

func parseFetchArgs(packets []pktPacket) fetchArgs {
	var args fetchArgs
	seenDelim := false
	for _, p := range packets {
		if p.kind == 'd' {
			seenDelim = true
			continue
		}
		if p.kind != 'D' {
			continue
		}
		line := packetText(p)
		if !seenDelim {
			continue
		}
		switch {
		case strings.HasPrefix(line, "want "):
			args.wants = append(args.wants, strings.TrimSuffix(line[5:], "\n"))
		case strings.HasPrefix(line, "have "):
			args.haves = append(args.haves, strings.TrimSuffix(line[5:], "\n"))
		case strings.HasPrefix(line, "shallow "):
			args.shallow = append(args.shallow, strings.TrimSuffix(line[8:], "\n"))
		case strings.HasPrefix(line, "deepen "):
			if f, ok := jsNumber(strings.TrimSuffix(line[7:], "\n")); ok && f == float64(int64(f)) && f > 0 {
				args.depth = int(f)
				args.hasDepth = true
			}
		}
	}
	return args
}

// shallowClosure: every object reachable from roots without following commit
// parents (commit -> tree, tag -> target, tree -> entries).
func shallowClosure(roots []string, lookup lookupFunc) map[string]bool {
	seen := make(map[string]bool)
	var queue []string
	for _, root := range roots {
		if isOID(root) {
			queue = append(queue, root)
		}
	}
	for len(queue) > 0 {
		oid := queue[len(queue)-1]
		queue = queue[:len(queue)-1]
		if seen[oid] {
			continue
		}
		obj := lookup(oid)
		if obj == nil {
			continue
		}
		seen[oid] = true
		switch obj.typ {
		case objCommit:
			queue = append(queue, parseCommit(obj.data).tree)
		case objTag:
			queue = append(queue, parseTag(obj.data).object)
		case objTree:
			entries, err := parseTree(obj.data)
			if err != nil {
				continue
			}
			for _, e := range entries {
				if e.mode != "160000" {
					queue = append(queue, e.oid)
				}
			}
		}
	}
	return seen
}

// haveClosure: the closure of the client's haves, stopping at the client's
// shallow boundaries (a shallow commit's parents are treated as not held).
func haveClosure(roots []string, shallow []string, lookup lookupFunc) map[string]bool {
	shallowSet := make(map[string]bool, len(shallow))
	for _, s := range shallow {
		shallowSet[s] = true
	}
	seen := make(map[string]bool)
	var queue []string
	for _, root := range roots {
		if isOID(root) {
			queue = append(queue, root)
		}
	}
	for len(queue) > 0 {
		oid := queue[len(queue)-1]
		queue = queue[:len(queue)-1]
		if seen[oid] {
			continue
		}
		obj := lookup(oid)
		if obj == nil {
			continue
		}
		seen[oid] = true
		switch obj.typ {
		case objCommit:
			commit := parseCommit(obj.data)
			queue = append(queue, commit.tree)
			if !shallowSet[oid] {
				queue = append(queue, commit.parents...)
			}
		case objTag:
			queue = append(queue, parseTag(obj.data).object)
		case objTree:
			entries, err := parseTree(obj.data)
			if err != nil {
				continue
			}
			for _, e := range entries {
				if e.mode != "160000" {
					queue = append(queue, e.oid)
				}
			}
		}
	}
	return seen
}

type fetchResult struct {
	objects   []gitObject
	shallow   []string
	unshallow []string
	acks      []string
}

// computeFetch mirrors the Node forge's fetch computation exactly.
func computeFetch(store *store, args fetchArgs) fetchResult {
	lookup := store.lookupFunc()
	var commitRoots []string
	var tagObjects []string
	for _, want := range args.wants {
		obj := lookup(want)
		if obj == nil {
			continue
		}
		if obj.typ == objTag {
			tagObjects = append(tagObjects, want)
			target := parseTag(obj.data).object
			if t := lookup(target); t != nil && t.typ == objCommit {
				commitRoots = append(commitRoots, target)
			}
		} else if obj.typ == objCommit {
			commitRoots = append(commitRoots, want)
		}
	}
	// BFS over parents from the roots, tracking distance.
	dist := make(map[string]int)
	type qitem struct {
		oid string
		d   int
	}
	queue := make([]qitem, 0, len(commitRoots))
	for _, r := range commitRoots {
		queue = append(queue, qitem{r, 0})
	}
	bfsCommits := make(map[string]bool)
	for len(queue) > 0 {
		item := queue[0]
		queue = queue[1:]
		if _, ok := dist[item.oid]; ok {
			continue
		}
		dist[item.oid] = item.d
		obj := lookup(item.oid)
		if obj == nil || obj.typ != objCommit {
			continue
		}
		if args.hasDepth && item.d >= args.depth {
			continue
		}
		bfsCommits[item.oid] = true
		for _, p := range parseCommit(obj.data).parents {
			queue = append(queue, qitem{p, item.d + 1})
		}
	}
	haveReachable := haveClosure(args.haves, args.shallow, lookup)
	var roots []string
	for oid := range bfsCommits {
		roots = append(roots, oid)
	}
	roots = append(roots, tagObjects...)
	wantReachable := shallowClosure(roots, lookup)
	var sendOids []string
	for oid := range wantReachable {
		if !haveReachable[oid] {
			sendOids = append(sendOids, oid)
		}
	}
	sort.Strings(sendOids)
	objects := make([]gitObject, 0, len(sendOids))
	for _, oid := range sendOids {
		if obj := lookup(oid); obj != nil {
			objects = append(objects, *obj)
		}
	}
	sentCommits := make(map[string]bool)
	for _, oid := range sendOids {
		if obj := lookup(oid); obj != nil && obj.typ == objCommit {
			sentCommits[oid] = true
		}
	}
	var acks []string
	for _, have := range args.haves {
		if obj := lookup(have); obj != nil && obj.typ == objCommit {
			acks = append(acks, have)
		}
	}
	var shallow, unshallow []string
	if args.hasDepth || len(args.shallow) > 0 {
		heldAfter := make(map[string]bool)
		for oid := range sentCommits {
			heldAfter[oid] = true
		}
		for _, h := range args.haves {
			heldAfter[h] = true
		}
		for oid := range heldAfter {
			obj := lookup(oid)
			if obj == nil || obj.typ != objCommit {
				continue
			}
			parents := parseCommit(obj.data).parents
			if len(parents) == 0 {
				continue
			}
			for _, p := range parents {
				if !heldAfter[p] {
					shallow = append(shallow, oid)
					break
				}
			}
		}
		sort.Strings(shallow)
		for _, oid := range args.shallow {
			obj := lookup(oid)
			if obj == nil || obj.typ != objCommit {
				continue
			}
			parents := parseCommit(obj.data).parents
			if len(parents) == 0 {
				continue
			}
			for _, p := range parents {
				if sentCommits[p] {
					unshallow = append(unshallow, oid)
					break
				}
			}
		}
		sort.Strings(unshallow)
	}
	return fetchResult{objects: objects, shallow: shallow, unshallow: unshallow, acks: acks}
}

func sidebandChunks(pack []byte) [][]byte {
	var chunks [][]byte
	for offset := 0; offset < len(pack); offset += 32768 {
		end := offset + 32768
		if end > len(pack) {
			end = len(pack)
		}
		chunk := make([]byte, 0, 1+end-offset)
		chunk = append(chunk, 1)
		chunk = append(chunk, pack[offset:end]...)
		chunks = append(chunks, pktLine(chunk))
	}
	return chunks
}

func handleUploadPack(c *ctx, repo *repo, isV2 bool) bool {
	packets, _ := decodePktLines(c.body)
	var first *pktPacket
	for i := range packets {
		if packets[i].kind == 'D' {
			first = &packets[i]
			break
		}
	}
	if first == nil {
		c.resp = c.errJSON(400, "Bad request: no git command")
		return true
	}
	command := strings.TrimSuffix(packetText(*first), "\n")
	store := c.store

	if command == "command=ls-refs" {
		args := parseLsRefsArgs(packets)
		body := lsRefsResponse(store, repo, args)
		c.resp = gitResp(store, "application/x-git-upload-pack-result", body)
		return true
	}

	if command == "command=fetch" {
		args := parseFetchArgs(packets)
		// The fetch response is cached per store object-generation + the raw
		// request body (the response depends only on the object store).
		key := fetchKey{repoID: repo.id, gen: store.gen, body: string(c.body)}
		if cached, ok := store.fetchCache[key]; ok {
			c.resp = gitResp(store, "application/x-git-upload-pack-result", cached)
			return true
		}
		result := computeFetch(store, args)
		var body []byte
		if isV2 {
			var parts [][]byte
			parts = append(parts, pktLine([]byte("acknowledgments\n")))
			if len(result.acks) > 0 {
				for _, ack := range result.acks {
					parts = append(parts, pktLine([]byte("ACK "+ack+"\n")))
				}
				parts = append(parts, pktLine([]byte("ready\n")))
			} else {
				parts = append(parts, pktLine([]byte("NAK\n")))
			}
			parts = append(parts, pktFlush())
			if len(result.shallow) > 0 || len(result.unshallow) > 0 {
				parts = append(parts, pktLine([]byte("shallow-info\n")))
				for _, oid := range result.shallow {
					parts = append(parts, pktLine([]byte("shallow "+oid+"\n")))
				}
				for _, oid := range result.unshallow {
					parts = append(parts, pktLine([]byte("unshallow "+oid+"\n")))
				}
				parts = append(parts, pktFlush())
			}
			if len(result.objects) > 0 {
				pack := writePack(result.objects)
				parts = append(parts, pktLine([]byte("packfile\n")))
				parts = append(parts, sidebandChunks(pack)...)
				parts = append(parts, pktFlush())
			}
			body = concatBytes(parts)
		} else {
			var parts [][]byte
			if len(result.acks) > 0 {
				for _, ack := range result.acks {
					parts = append(parts, pktLine([]byte("ACK "+ack+"\n")))
				}
			} else {
				parts = append(parts, pktLine([]byte("NAK\n")))
			}
			for _, oid := range result.shallow {
				parts = append(parts, pktLine([]byte("shallow "+oid+"\n")))
			}
			for _, oid := range result.unshallow {
				parts = append(parts, pktLine([]byte("unshallow "+oid+"\n")))
			}
			if len(result.objects) > 0 {
				pack := writePack(result.objects)
				parts = append(parts, sidebandChunks(pack)...)
			}
			body = concatBytes(parts)
		}
		store.fetchCache[key] = body
		c.resp = gitResp(store, "application/x-git-upload-pack-result", body)
		return true
	}

	c.resp = c.errJSON(400, "Bad request: unknown git command")
	return true
}

// --- receive-pack ---

type pushCmd struct {
	oldOID string
	newOID string
	ref    string
}

func handleReceivePack(c *ctx, repo *repo) bool {
	store := c.store
	packets, rest := decodePktLines(c.body)
	var dataPackets []pktPacket
	for _, p := range packets {
		if p.kind == 'D' {
			dataPackets = append(dataPackets, p)
		}
	}
	if len(dataPackets) == 0 {
		c.resp = c.errJSON(400, "Bad request: no push commands")
		return true
	}
	var commands []pushCmd
	malformed := false
	for _, packet := range dataPackets {
		// Byte-level parse: "<old> <new> <ref>\n" with optional "\0caps" after
		// the ref. No string conversions until the three fields are known
		// valid (they become map keys, so they allocate once each anyway).
		payload := packet.payload
		if len(payload) > 0 && payload[len(payload)-1] == '\n' {
			payload = payload[:len(payload)-1]
		}
		if i := bytes.IndexByte(payload, 0); i >= 0 {
			payload = payload[:i]
		}
		sp1 := bytes.IndexByte(payload, ' ')
		sp2 := -1
		if sp1 >= 0 {
			if j := bytes.IndexByte(payload[sp1+1:], ' '); j >= 0 {
				sp2 = sp1 + 1 + j
			}
		}
		if sp1 < 0 || sp2 < 0 || !isOIDBytes(payload[:sp1]) || !isOIDBytes(payload[sp1+1:sp2]) || !bytes.HasPrefix(payload[sp2+1:], []byte("refs/")) {
			malformed = true
			break
		}
		commands = append(commands, pushCmd{oldOID: string(payload[:sp1]), newOID: string(payload[sp1+1 : sp2]), ref: string(payload[sp2+1:])})
	}

	var reportLines []string
	unpackOK := true
	if len(commands) == 0 {
		unpackOK = false
		reportLines = append(reportLines, "unpack error no commands")
	} else if malformed {
		unpackOK = false
		reportLines = append(reportLines, "unpack error malformed command")
		for _, cmd := range commands {
			reportLines = append(reportLines, "ng "+cmd.ref+" malformed command")
		}
	} else {
		needsPack := false
		for _, cmd := range commands {
			if cmd.newOID != zeroOID {
				needsPack = true
				break
			}
		}
		if needsPack && len(rest) > 0 {
			objects, err := readPack(rest, store.lookupFunc())
			if err != nil {
				unpackOK = false
				reportLines = append(reportLines, "unpack error "+err.Error())
			} else {
				for oid, obj := range objects {
					if _, ok := store.objects[oid]; !ok {
						store.objects[oid] = obj
						store.gen++
					}
				}
			}
		} else if needsPack && len(rest) == 0 {
			unpackOK = false
			reportLines = append(reportLines, "unpack error missing pack")
		}
		if unpackOK {
			reportLines = append(reportLines, "unpack ok")
			for _, cmd := range commands {
				current, exists := repo.refs[cmd.ref]
				if cmd.newOID == zeroOID {
					// delete
					if !exists {
						reportLines = append(reportLines, "ng "+cmd.ref+" ref does not exist")
					} else if cmd.oldOID != zeroOID && cmd.oldOID != current {
						reportLines = append(reportLines, "ng "+cmd.ref+" old value does not match")
					} else {
						repo.delRef(cmd.ref)
						repo.bumpRefs()
						reportLines = append(reportLines, "ok "+cmd.ref)
					}
				} else if cmd.oldOID == zeroOID {
					// create
					if exists {
						reportLines = append(reportLines, "ng "+cmd.ref+" already exists")
					} else if store.lookup(cmd.newOID) == nil {
						reportLines = append(reportLines, "ng "+cmd.ref+" missing object "+cmd.newOID)
					} else {
						repo.setRef(cmd.ref, cmd.newOID)
						repo.bumpRefs()
						reportLines = append(reportLines, "ok "+cmd.ref)
					}
				} else {
					// update
					if !exists {
						reportLines = append(reportLines, "ng "+cmd.ref+" ref does not exist")
					} else if cmd.oldOID != current {
						reportLines = append(reportLines, "ng "+cmd.ref+" old value does not match")
					} else if store.lookup(cmd.newOID) == nil {
						reportLines = append(reportLines, "ng "+cmd.ref+" missing object "+cmd.newOID)
					} else if !isDescendant(cmd.newOID, cmd.oldOID, store.lookupFunc()) {
						reportLines = append(reportLines, "ng "+cmd.ref+" non-fast-forward")
					} else {
						repo.setRef(cmd.ref, cmd.newOID)
						repo.bumpRefs()
						reportLines = append(reportLines, "ok "+cmd.ref)
					}
				}
			}
			ts := store.nextTS()
			repo.updatedAt = isoOf(ts)
			repo.pushedAt = isoOf(ts)
		} else {
			for _, cmd := range commands {
				reportLines = append(reportLines, "ng "+cmd.ref+" unpack failed")
			}
		}
	}

	// Assemble the sideband report in one buffer: pkt-line( 0x01 +
	// pkt-line(line)+ ... + flush ) + flush. (Previously this built a
	// [][]byte of per-line pkt-lines plus two concatBytes passes.)
	reportLen := 0
	for _, l := range reportLines {
		reportLen += 4 + len(l) + 1
	}
	reportLen += 4 // flush
	inner := make([]byte, 0, 1+reportLen)
	inner = append(inner, 1)
	for _, l := range reportLines {
		inner = appendPktLine(inner, l)
	}
	inner = append(inner, "0000"...)
	body := make([]byte, 0, 4+len(inner)+4)
	body = appendPktLineBytes(body, inner)
	body = append(body, "0000"...)
	c.resp = gitResp(store, "application/x-git-receive-pack-result", body)
	return true
}
